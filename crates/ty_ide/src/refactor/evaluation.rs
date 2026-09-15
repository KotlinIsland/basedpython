//! When an expression is evaluated, relative to the statement it is part of.
//!
//! Moving an expression — hoisting it into a variable above its statement, or
//! inlining a variable's value into the place that reads it — keeps the program
//! meaning the same only when the expression is evaluated exactly once each time
//! its statement runs, and when nothing evaluated before it could observe the
//! move. This module answers both, following python's evaluation order.

use ruff_python_ast::{self as ast, AnyNodeRef, Expr, Stmt, UnaryOp};
use ruff_text_size::Ranged;

/// Where a target expression sits in the evaluation of its statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placement {
    /// The target is evaluated exactly once whenever the statement runs.
    /// `effects_before` says whether something evaluated before it may have an
    /// effect: call a function, run a dunder, bind a name.
    Once { effects_before: bool },
    /// The target is not evaluated exactly once per run of the statement.
    NotOnce(NotOnce),
}

/// Why an expression is not evaluated exactly once per run of its statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotOnce {
    Conditional,
    Loop,
    Deferred,
    Comprehension,
    Body,
    Annotation,
    Assertion,
    OptionalChain,
    Opaque,
}

impl NotOnce {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            NotOnce::Conditional => "it is only evaluated when a condition holds",
            NotOnce::Loop => "it is evaluated again on every iteration of the loop",
            NotOnce::Deferred => "it is evaluated later, when the enclosing function is called",
            NotOnce::Comprehension => "it is evaluated once per element of a comprehension",
            NotOnce::Body => "it is inside the body of a compound statement",
            NotOnce::Annotation => "it is part of an annotation",
            NotOnce::Assertion => "an `assert` is not evaluated when python runs with `-O`",
            NotOnce::OptionalChain => {
                "it is a link of an optional chain, which is evaluated as a whole"
            }
            NotOnce::Opaque => "it is part of a construct whose evaluation order is not modelled",
        }
    }
}

/// Whether evaluating `expr` is free of effects and gives the same value no
/// matter where it is evaluated, as long as the names it reads are not rebound.
///
/// This is deliberately narrow: a name, an immutable literal, a negated number
/// and a tuple of those. An attribute can be a property, an operator can be a
/// dunder, and a list display makes a new list each time it is evaluated.
pub(crate) fn is_pure(expr: &Expr) -> bool {
    match expr {
        Expr::Name(name) => name.ctx.is_load(),
        Expr::StringLiteral(_)
        | Expr::BytesLiteral(_)
        | Expr::NumberLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_)
        | Expr::EllipsisLiteral(_) => true,
        Expr::UnaryOp(unary) => {
            matches!(unary.op, UnaryOp::USub | UnaryOp::UAdd | UnaryOp::Invert)
                && unary.operand.is_number_literal_expr()
        }
        Expr::Tuple(tuple) => {
            tuple.ctx.is_load()
                && !tuple.is_anon_named_tuple
                && !tuple.is_anon_named_tuple_value
                && tuple.callable_shape.is_none()
                && !tuple.is_parameter_shape
                && tuple.elts.iter().all(is_pure)
        }
        _ => false,
    }
}

/// Where the expression `target` sits in the evaluation of `statement`. `target`
/// must be a node of `statement`'s tree, since it is found by identity.
pub(crate) fn placement_in_statement(statement: &Stmt, target: AnyNodeRef<'_>) -> Placement {
    let mut walk = Walk {
        target,
        effects: false,
    };
    walk.statement(statement)
        .unwrap_or(Placement::NotOnce(NotOnce::Opaque))
}

struct Walk<'a> {
    target: AnyNodeRef<'a>,
    effects: bool,
}

type Found = Option<Placement>;

impl Walk<'_> {
    fn contains(&self, node: impl Ranged) -> bool {
        node.range().contains_range(self.target.range())
    }

    /// Something not modelled was evaluated: the target inside it is refused,
    /// and a target after it has an effect before it.
    fn opaque(&mut self, node: impl Ranged, reason: NotOnce) -> Found {
        if self.contains(node) {
            return Some(Placement::NotOnce(reason));
        }
        self.effects = true;
        None
    }

    /// An expression evaluated at most once, or not at all, after what has
    /// been walked so far.
    fn not_once(&mut self, expr: &Expr, reason: NotOnce) -> Found {
        if self.contains(expr) {
            return Some(Placement::NotOnce(reason));
        }
        if !is_pure(expr) {
            self.effects = true;
        }
        None
    }

    fn not_once_all<'e>(
        &mut self,
        exprs: impl IntoIterator<Item = &'e Expr>,
        reason: NotOnce,
    ) -> Found {
        for expr in exprs {
            if let Some(found) = self.not_once(expr, reason) {
                return Some(found);
            }
        }
        None
    }

    fn exprs<'e>(&mut self, exprs: impl IntoIterator<Item = &'e Expr>) -> Found {
        for expr in exprs {
            if let Some(found) = self.expr(expr) {
                return Some(found);
            }
        }
        None
    }

    fn effect(&mut self) {
        self.effects = true;
    }

    fn statement(&mut self, statement: &Stmt) -> Found {
        match statement {
            Stmt::Expr(stmt) => self.expr(&stmt.value),
            Stmt::Return(stmt) => stmt.value.as_deref().and_then(|value| self.expr(value)),
            Stmt::Assign(stmt) => {
                if !stmt.decorator_list.is_empty() {
                    return self.opaque(stmt, NotOnce::Opaque);
                }
                if let Some(found) = self.expr(&stmt.value) {
                    return Some(found);
                }
                for target in &stmt.targets {
                    if let Some(found) = self.target(target) {
                        return Some(found);
                    }
                }
                None
            }
            Stmt::AugAssign(stmt) => {
                // the target is read, the value evaluated, the operator run and
                // the result stored. reading a name has no effect; reading an
                // attribute or an item does
                if !stmt.target.is_name_expr() {
                    if let Some(found) = self.target(&stmt.target) {
                        return Some(found);
                    }
                    self.effect();
                }
                let found = self.expr(&stmt.value);
                self.effect();
                found
            }
            Stmt::AnnAssign(stmt) => {
                if !stmt.decorator_list.is_empty() || stmt.is_context {
                    return self.opaque(stmt, NotOnce::Opaque);
                }
                if self.contains(&*stmt.annotation) {
                    return Some(Placement::NotOnce(NotOnce::Annotation));
                }
                if let Some(value) = &stmt.value
                    && let Some(found) = self.expr(value)
                {
                    return Some(found);
                }
                self.target(&stmt.target)
            }
            Stmt::If(stmt) => {
                if stmt.pattern.is_some() {
                    return self.opaque(stmt, NotOnce::Opaque);
                }
                if let Some(found) = self.expr(&stmt.test) {
                    return Some(found);
                }
                for clause in &stmt.elif_else_clauses {
                    if let Some(test) = &clause.test
                        && self.contains(test)
                    {
                        return Some(Placement::NotOnce(NotOnce::Conditional));
                    }
                }
                self.contains(stmt)
                    .then_some(Placement::NotOnce(NotOnce::Body))
            }
            Stmt::While(stmt) => {
                self.contains(stmt)
                    .then_some(Placement::NotOnce(if self.contains(&*stmt.test) {
                        NotOnce::Loop
                    } else {
                        NotOnce::Body
                    }))
            }
            Stmt::For(stmt) => {
                if let Some(found) = self.expr(&stmt.iter) {
                    return Some(found);
                }
                self.contains(stmt)
                    .then_some(Placement::NotOnce(NotOnce::Body))
            }
            Stmt::With(stmt) => {
                for item in &stmt.items {
                    if let Some(found) = self.expr(&item.context_expr) {
                        return Some(found);
                    }
                    // `__enter__` runs before the next item is evaluated
                    self.effect();
                    if let Some(vars) = &item.optional_vars
                        && self.contains(&**vars)
                    {
                        return Some(Placement::NotOnce(NotOnce::Opaque));
                    }
                }
                self.contains(stmt)
                    .then_some(Placement::NotOnce(NotOnce::Body))
            }
            Stmt::Match(stmt) => {
                if let Some(found) = self.expr(&stmt.subject) {
                    return Some(found);
                }
                self.contains(stmt)
                    .then_some(Placement::NotOnce(NotOnce::Body))
            }
            Stmt::Raise(stmt) => {
                if let Some(exc) = &stmt.exc
                    && let Some(found) = self.expr(exc)
                {
                    return Some(found);
                }
                stmt.cause.as_deref().and_then(|cause| self.expr(cause))
            }
            Stmt::Assert(stmt) => self
                .contains(stmt)
                .then_some(Placement::NotOnce(NotOnce::Assertion)),
            Stmt::Delete(stmt) => {
                for target in &stmt.targets {
                    if let Some(found) = self.target(target) {
                        return Some(found);
                    }
                }
                None
            }
            Stmt::FunctionDef(_)
            | Stmt::ClassDef(_)
            | Stmt::TypeAlias(_)
            | Stmt::Let(_)
            | Stmt::Try(_)
            | Stmt::Import(_)
            | Stmt::ImportFrom(_)
            | Stmt::Global(_)
            | Stmt::Nonlocal(_)
            | Stmt::Pass(_)
            | Stmt::Break(_)
            | Stmt::Continue(_)
            | Stmt::IpyEscapeCommand(_) => self.opaque(statement, NotOnce::Opaque),
        }
    }

    /// The parts of an assignment target that are evaluated before the store.
    fn target(&mut self, target: &Expr) -> Found {
        match target {
            Expr::Name(_) => {
                // the store binds the name before any later target is evaluated
                self.effect();
                None
            }
            Expr::Attribute(attribute) if !attribute.optional => {
                let found = self.expr(&attribute.value);
                self.effect();
                found
            }
            Expr::Subscript(subscript) if !subscript.is_typeof && !subscript.is_type_decoration => {
                if let Some(found) = self.expr(&subscript.value) {
                    return Some(found);
                }
                let found = self.expr(&subscript.slice);
                self.effect();
                found
            }
            Expr::Tuple(tuple) => {
                self.effect();
                for element in &tuple.elts {
                    if let Some(found) = self.target(element) {
                        return Some(found);
                    }
                }
                None
            }
            Expr::List(list) => {
                self.effect();
                for element in &list.elts {
                    if let Some(found) = self.target(element) {
                        return Some(found);
                    }
                }
                None
            }
            Expr::Starred(starred) => self.target(&starred.value),
            _ => self.opaque(target, NotOnce::Opaque),
        }
    }

    fn expr(&mut self, expr: &Expr) -> Found {
        if AnyNodeRef::from(expr).ptr_eq(self.target) {
            return Some(Placement::Once {
                effects_before: self.effects,
            });
        }
        if !self.contains(expr) {
            if !is_pure(expr) {
                self.effect();
            }
            return None;
        }

        match expr {
            Expr::BoolOp(bool_op) => {
                let (first, rest) = bool_op.values.split_first()?;
                if let Some(found) = self.expr(first) {
                    return Some(found);
                }
                self.not_once_all(rest, NotOnce::Conditional)
            }
            Expr::Named(named) => {
                let found = self.expr(&named.value);
                self.effect();
                found
            }
            Expr::BinOp(bin_op) => match bin_op.op {
                ast::Operator::Coalesce => {
                    if let Some(found) = self.expr(&bin_op.left) {
                        return Some(found);
                    }
                    self.not_once(&bin_op.right, NotOnce::Conditional)
                }
                ast::Operator::Result => self.opaque(expr, NotOnce::Opaque),
                _ => {
                    if let Some(found) = self.exprs([&*bin_op.left, &*bin_op.right]) {
                        return Some(found);
                    }
                    self.effect();
                    None
                }
            },
            Expr::UnaryOp(unary) => match unary.op {
                UnaryOp::Optional | UnaryOp::Propagate | UnaryOp::Force => {
                    self.opaque(expr, NotOnce::Opaque)
                }
                UnaryOp::Not | UnaryOp::Invert | UnaryOp::UAdd | UnaryOp::USub => {
                    let found = self.expr(&unary.operand);
                    self.effect();
                    found
                }
            },
            Expr::Lambda(_) => Some(Placement::NotOnce(NotOnce::Deferred)),
            Expr::If(if_expr) => {
                if let Some(found) = self.expr(&if_expr.test) {
                    return Some(found);
                }
                self.not_once_all([&*if_expr.body, &*if_expr.orelse], NotOnce::Conditional)
            }
            Expr::Dict(dict) => {
                for item in &dict.items {
                    if let Some(key) = &item.key
                        && let Some(found) = self.expr(key)
                    {
                        return Some(found);
                    }
                    if let Some(found) = self.expr(&item.value) {
                        return Some(found);
                    }
                }
                None
            }
            Expr::Set(set) => self.exprs(&set.elts),
            Expr::List(list) => self.exprs(&list.elts),
            Expr::Tuple(tuple) => {
                if tuple.is_anon_named_tuple
                    || tuple.is_anon_named_tuple_value
                    || tuple.callable_shape.is_some()
                    || tuple.is_parameter_shape
                {
                    return self.opaque(expr, NotOnce::Opaque);
                }
                self.exprs(&tuple.elts)
            }
            Expr::ListComp(comp) => self.comprehension(&comp.generators),
            Expr::SetComp(comp) => self.comprehension(&comp.generators),
            Expr::DictComp(comp) => self.comprehension(&comp.generators),
            Expr::Generator(comp) => self.comprehension(&comp.generators),
            Expr::Await(await_expr) => {
                let found = self.expr(&await_expr.value);
                self.effect();
                found
            }
            Expr::Yield(yield_expr) => {
                let found = yield_expr
                    .value
                    .as_deref()
                    .and_then(|value| self.expr(value));
                self.effect();
                found
            }
            Expr::YieldFrom(yield_from) => {
                let found = self.expr(&yield_from.value);
                self.effect();
                found
            }
            Expr::Compare(compare) => {
                if let Some(found) = self.expr(&compare.left) {
                    return Some(found);
                }
                let (first, rest) = compare.comparators.split_first()?;
                if let Some(found) = self.expr(first) {
                    return Some(found);
                }
                self.effect();
                // a chained comparison stops at the first comparison that fails
                self.not_once_all(rest, NotOnce::Conditional)
            }
            Expr::Call(call) => {
                if call.cast_kind.is_some() || call.is_string_tag {
                    return self.opaque(expr, NotOnce::Opaque);
                }
                if is_optional_chain(expr) {
                    return self.opaque(expr, NotOnce::OptionalChain);
                }
                if let Some(found) = self.expr(&call.func) {
                    return Some(found);
                }
                for argument in call.arguments.iter_source_order() {
                    if let Some(found) = self.expr(argument.value()) {
                        return Some(found);
                    }
                }
                self.effect();
                None
            }
            Expr::Attribute(attribute) => {
                if is_optional_chain(expr) {
                    return self.opaque(expr, NotOnce::OptionalChain);
                }
                let found = self.expr(&attribute.value);
                self.effect();
                found
            }
            Expr::Subscript(subscript) => {
                if subscript.is_typeof || subscript.is_type_decoration {
                    return self.opaque(expr, NotOnce::Opaque);
                }
                if is_optional_chain(expr) {
                    return self.opaque(expr, NotOnce::OptionalChain);
                }
                if let Some(found) = self.exprs([&*subscript.value, &*subscript.slice]) {
                    return Some(found);
                }
                self.effect();
                None
            }
            Expr::Starred(starred) => {
                let found = self.expr(&starred.value);
                self.effect();
                found
            }
            Expr::Slice(slice) => self.exprs(
                [&slice.lower, &slice.upper, &slice.step]
                    .into_iter()
                    .filter_map(|part| part.as_deref()),
            ),
            // a leaf that contains the target is the target, which was handled above
            Expr::Name(_)
            | Expr::StringLiteral(_)
            | Expr::BytesLiteral(_)
            | Expr::NumberLiteral(_)
            | Expr::BooleanLiteral(_)
            | Expr::NoneLiteral(_)
            | Expr::EllipsisLiteral(_) => None,
            // an f-string's interpolations are formatted as they are reached, but
            // the target inside one is left where it is
            Expr::FString(_)
            | Expr::TString(_)
            | Expr::IpyEscapeCommand(_)
            | Expr::CallableType(_)
            | Expr::ProtocolType(_)
            | Expr::ProtocolMethod(_)
            | Expr::Statement(_) => self.opaque(expr, NotOnce::Opaque),
        }
    }

    /// Only the first iterable of a comprehension is evaluated in the enclosing
    /// scope, once; everything else is evaluated per element.
    fn comprehension(&mut self, generators: &[ast::Comprehension]) -> Found {
        let (first, _) = generators.split_first()?;
        if let Some(found) = self.expr(&first.iter) {
            return Some(found);
        }
        Some(Placement::NotOnce(NotOnce::Comprehension))
    }
}

/// Whether `expr` is part of a basedpython optional chain: a `?.` access, or a
/// trailer applied to one. A chain is evaluated as a whole — an absent receiver
/// skips every trailer after it — so no link of it is evaluated on its own.
fn is_optional_chain(expr: &Expr) -> bool {
    match expr {
        Expr::Attribute(attribute) => attribute.optional || is_optional_chain(&attribute.value),
        Expr::Call(call) => is_optional_chain(&call.func),
        Expr::Subscript(subscript) => is_optional_chain(&subscript.value),
        _ => false,
    }
}
