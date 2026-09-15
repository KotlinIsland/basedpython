//! Inline Variable: replace every read of a variable with the value it is
//! assigned, and remove the assignment.
//!
//! The reads are the ones the semantic index resolves to the assignment, so a
//! same-named parameter of another function, an attribute, a keyword argument,
//! a string or a comment is never touched. The rewrite is refused unless every
//! read is guaranteed to see that assignment and nothing else, and unless moving
//! the value to each read cannot change when — or how often — it is evaluated.

use ruff_diagnostics::Edit;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::source_order::{
    SourceOrderVisitor, TraversalSignal, walk_annotation, walk_node,
};
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, OperatorPrecedence, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::scope::{FileScopeId, NodeWithScopeKind, ScopeKind};
use ty_python_core::{SemanticIndex, semantic_index};

use super::evaluation::{NotOnce, Placement, is_pure, placement_in_statement};
use super::flow::{
    bindings_of_symbol, reaching_definitions, rebound_from_nested_scope, resolving_scope,
};
use super::hazards::{contains_named_expression, context_dependent_construct};
use super::text::{statement_ancestors, suite_of};
use super::{Plan, RefactorContext, Refusal};
use crate::goto::find_goto_target;
use crate::references::{ReferencesMode, references};

pub(super) fn plan(context: &RefactorContext<'_>, range: TextRange) -> Result<Plan, Refusal> {
    let db = context.db;
    let module = context.parsed.syntax();
    if !module.range().contains_range(range) {
        return Err(Refusal::NotApplicable);
    }
    let covering = covering_node(module.into(), range);
    let AnyNodeRef::ExprName(selected) = covering.node() else {
        return Err(Refusal::NotApplicable);
    };
    let index = semantic_index(db, context.file);
    let name = selected.id.clone();
    let title = format!("Inline variable `{name}`");
    let refuse = |reason: &str| Refusal::refused(title.clone(), reason.to_string());

    let Some(use_scope) = index.try_expression_scope_id(&ast::ExprRef::Name(selected)) else {
        return Err(Refusal::NotApplicable);
    };
    let Some(scope) = resolving_scope(index, use_scope, &name) else {
        return Err(Refusal::NotApplicable);
    };
    let Some(symbol) = index.place_table(scope).symbol_id(&name) else {
        return Err(Refusal::NotApplicable);
    };

    let in_stub = context.file().is_stub(db);
    let bindings = bindings_of_symbol(db, index, &context.parsed, scope, symbol, in_stub);
    let [definition] = bindings.as_slice() else {
        return Err(
            if bindings
                .iter()
                .any(|binding| is_inlinable_kind(*binding, context))
            {
                refuse(&format!("`{name}` is assigned more than once"))
            } else {
                Refusal::NotApplicable
            },
        );
    };
    let definition = *definition;
    let Some(assignment) = Assignment::of(definition, context) else {
        return Err(Refusal::NotApplicable);
    };

    match index.scope(scope).kind() {
        ScopeKind::Function | ScopeKind::Module => {}
        ScopeKind::Class => {
            return Err(refuse(&format!(
                "`{name}` is a class attribute, which can be read through the class and its instances"
            )));
        }
        _ => return Err(Refusal::NotApplicable),
    }
    let table = index.place_table(scope);
    let symbol_flags = table.symbol(symbol);
    if symbol_flags.is_global() || symbol_flags.is_nonlocal() {
        return Err(Refusal::NotApplicable);
    }
    if rebound_from_nested_scope(index, scope, &name) {
        return Err(refuse(&format!(
            "`{name}` is rebound by a nested function's `global` or `nonlocal`"
        )));
    }

    let statement = assignment.statement;
    let Some(in_suite) = suite_of(context.parsed.suite(), statement) else {
        return Err(Refusal::NotApplicable);
    };
    if !context.owns_its_lines(statement) {
        return Err(refuse(
            "the assignment shares its line with another statement",
        ));
    }

    let value = assignment.value;
    if let Some(reason) = context_dependent_construct(context, AnyNodeRef::from(value)) {
        return Err(refuse(&format!("its value can't be moved: {reason}")));
    }
    if contains_named_expression(value) {
        return Err(refuse("its value binds a name with `:=`"));
    }

    let reads = collect_reads(context, index, scope, &name)?;
    let reads = match reads {
        Ok(reads) => reads,
        Err(reason) => return Err(refuse(&reason)),
    };
    if reads.is_empty() {
        return Err(refuse(&format!("`{name}` is never read")));
    }

    // a module's variable is also an attribute other modules can import
    if scope.is_global() {
        let offset = assignment.target.start();
        let Some(target) = find_goto_target(&context.model, &context.parsed, offset) else {
            return Err(Refusal::NotApplicable);
        };
        let found =
            references(db, context.file, &target, ReferencesMode::References).unwrap_or_default();
        if found
            .iter()
            .any(|reference| reference.file() != context.file())
        {
            return Err(refuse(&format!("`{name}` is imported by another module")));
        }
    }

    for read in &reads {
        if read.in_annotation {
            return Err(refuse(&format!(
                "`{name}` is read in an annotation, where its value would be read as a type"
            )));
        }
        if read.scope == scope
            && !reaching_definitions(db, index, scope, read.name).is_exactly(definition)
        {
            return Err(refuse(&format!(
                "a read of `{name}` may see another value, or none"
            )));
        }
    }

    let pure = is_pure(value);
    if pure {
        check_pure_value_is_stable(context, index, scope, &assignment, &reads, &in_suite)
            .map_err(|reason| refuse(&reason))?;
    } else {
        check_single_immediate_read(&reads, scope, &in_suite).map_err(|reason| refuse(&reason))?;
    }

    let value_text = context.expression_text(value);
    let mut edits = Vec::with_capacity(reads.len() + 1);
    for read in &reads {
        if read.in_interpolation && value_text.contains(['"', '\'', '\\', '\n', '\r']) {
            return Err(refuse(
                "its value would be written inside an f-string, where its quotes or line breaks may not be allowed",
            ));
        }
        let parenthesize =
            read.in_interpolation || needs_parentheses(value, read.parent, read.name, context);
        let replacement = if parenthesize {
            format!("({value_text})")
        } else {
            value_text.to_string()
        };
        edits.push(Edit::range_replacement(replacement, read.name.range()));
    }
    edits.push(context.delete_statement(in_suite));

    Ok(Plan { title, edits })
}

/// Whether `definition` is a binding this refactoring could inline, to tell a
/// variable that is also assigned elsewhere from a name bound some other way.
fn is_inlinable_kind(definition: Definition<'_>, context: &RefactorContext<'_>) -> bool {
    Assignment::of(definition, context).is_some()
}

/// A `name = value` the refactoring can remove.
struct Assignment<'a> {
    statement: &'a Stmt,
    target: &'a ast::ExprName,
    value: &'a Expr,
}

impl<'a> Assignment<'a> {
    fn of(definition: Definition<'_>, context: &'a RefactorContext<'_>) -> Option<Self> {
        let db = context.db;
        let parsed = &context.parsed;
        let target_range = match definition.kind(db) {
            DefinitionKind::Assignment(assignment) => {
                if !assignment.is_sole_target() || assignment.unpack().is_some() {
                    return None;
                }
                assignment.target(parsed).range()
            }
            DefinitionKind::AnnotatedAssignment(assignment) => assignment.target(parsed).range(),
            _ => return None,
        };
        let statement = *statement_ancestors(parsed.suite(), target_range).last()?;
        let (target, value) = match statement {
            Stmt::Assign(assign)
                if assign.targets.len() == 1 && assign.decorator_list.is_empty() =>
            {
                (&assign.targets[0], &*assign.value)
            }
            Stmt::AnnAssign(ann_assign)
                if !ann_assign.is_context && ann_assign.decorator_list.is_empty() =>
            {
                (&*ann_assign.target, ann_assign.value.as_deref()?)
            }
            _ => return None,
        };
        let target = target.as_name_expr()?;
        (target.range() == target_range).then_some(Assignment {
            statement,
            target,
            value,
        })
    }
}

/// A load of the variable being inlined.
struct Read<'a> {
    name: &'a ast::ExprName,
    parent: AnyNodeRef<'a>,
    scope: FileScopeId,
    in_annotation: bool,
    in_interpolation: bool,
}

/// Every load of `name` that resolves to `scope`'s variable. `Err(Ok(reason))`
/// refuses; the outer `Err` is reserved for not applying at all.
fn collect_reads<'a>(
    context: &'a RefactorContext<'_>,
    index: &SemanticIndex<'_>,
    scope: FileScopeId,
    name: &Name,
) -> Result<Result<Vec<Read<'a>>, String>, Refusal> {
    let root: AnyNodeRef<'a> = match index.scope(scope).node() {
        NodeWithScopeKind::Module => context.parsed.syntax().into(),
        NodeWithScopeKind::Function(function) => function.node(&context.parsed).into(),
        _ => return Err(Refusal::NotApplicable),
    };

    let mut collector = ReadCollector {
        index,
        scope,
        name,
        ancestors: Vec::new(),
        annotation_depth: 0,
        interpolation_depth: 0,
        reads: Vec::new(),
        refusal: None,
        root,
    };
    walk_node(&mut collector, root);
    Ok(match collector.refusal {
        Some(reason) => Err(reason),
        None => Ok(collector.reads),
    })
}

struct ReadCollector<'a, 'i, 'db> {
    index: &'i SemanticIndex<'db>,
    scope: FileScopeId,
    name: &'i Name,
    ancestors: Vec<AnyNodeRef<'a>>,
    annotation_depth: usize,
    interpolation_depth: usize,
    reads: Vec<Read<'a>>,
    refusal: Option<String>,
    root: AnyNodeRef<'a>,
}

impl<'a> SourceOrderVisitor<'a> for ReadCollector<'a, '_, '_> {
    fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
        // `leave_node` runs for every node `enter_node` saw, skipped or not, so the
        // bookkeeping is done for all of them and undone there
        let skip = matches!(self.root, AnyNodeRef::StmtFunctionDef(_))
            && self.ancestors.len() == 1
            && !self.is_body_statement(node);
        if is_interpolated_string(node) {
            self.interpolation_depth += 1;
        }
        if is_type_definition(node) {
            self.annotation_depth += 1;
        }
        if !skip
            && let AnyNodeRef::ExprName(name) = node
            && name.id == *self.name
        {
            self.name_occurrence(name);
        }
        self.ancestors.push(node);
        // a function's decorators, defaults and annotations are evaluated in the
        // scope around it, so for a function's own variable only its body counts
        if skip {
            TraversalSignal::Skip
        } else {
            TraversalSignal::Traverse
        }
    }

    fn leave_node(&mut self, node: AnyNodeRef<'a>) {
        self.ancestors.pop();
        if is_interpolated_string(node) {
            self.interpolation_depth -= 1;
        }
        if is_type_definition(node) {
            self.annotation_depth -= 1;
        }
    }

    fn visit_annotation(&mut self, expr: &'a Expr) {
        self.annotation_depth += 1;
        walk_annotation(self, expr);
        self.annotation_depth -= 1;
    }
}

fn is_interpolated_string(node: AnyNodeRef<'_>) -> bool {
    matches!(
        node,
        AnyNodeRef::ExprFString(_) | AnyNodeRef::ExprTString(_)
    )
}

fn is_type_definition(node: AnyNodeRef<'_>) -> bool {
    matches!(
        node,
        AnyNodeRef::TypeParams(_) | AnyNodeRef::StmtTypeAlias(_)
    )
}

impl<'a> ReadCollector<'a, '_, '_> {
    fn is_body_statement(&self, node: AnyNodeRef<'_>) -> bool {
        let AnyNodeRef::StmtFunctionDef(function) = self.root else {
            return false;
        };
        function
            .body
            .iter()
            .any(|stmt| AnyNodeRef::from(stmt).ptr_eq(node))
    }

    fn name_occurrence(&mut self, name: &'a ast::ExprName) {
        let Some(use_scope) = self
            .index
            .try_expression_scope_id(&ast::ExprRef::Name(name))
        else {
            return;
        };
        if resolving_scope(self.index, use_scope, &name.id) != Some(self.scope) {
            return;
        }
        match name.ctx {
            ast::ExprContext::Load => {
                let Some(parent) = self.ancestors.last().copied() else {
                    return;
                };
                self.reads.push(Read {
                    name,
                    parent,
                    scope: use_scope,
                    in_annotation: self.annotation_depth > 0,
                    in_interpolation: self.interpolation_depth > 0,
                });
            }
            ast::ExprContext::Del => {
                self.refusal
                    .get_or_insert_with(|| format!("`{}` is deleted with `del`", name.id));
            }
            ast::ExprContext::Store | ast::ExprContext::Invalid => {}
        }
    }
}

/// A pure value reads names, and each read must still see what it saw at the
/// assignment. Every binding of those names in the variable's scope has to come
/// before the assignment and not be revisited by a loop around it; a read in a
/// nested function can run at any later time, so it is only allowed when the
/// assignment runs unconditionally, directly in the variable's scope.
fn check_pure_value_is_stable(
    context: &RefactorContext<'_>,
    index: &SemanticIndex<'_>,
    scope: FileScopeId,
    assignment: &Assignment<'_>,
    reads: &[Read<'_>],
    in_suite: &super::text::InSuite<'_>,
) -> Result<(), String> {
    let db = context.db;
    let statement_range = assignment.statement.range();
    let enclosing = statement_ancestors(context.parsed.suite(), statement_range);
    let loops: Vec<TextRange> = enclosing
        .iter()
        .filter(|stmt| matches!(stmt, Stmt::For(_) | Stmt::While(_)))
        .map(Ranged::range)
        .collect();

    let has_nested_read = reads.iter().any(|read| read.scope != scope);
    if has_nested_read {
        let top_level = match index.scope(scope).node() {
            NodeWithScopeKind::Module => context.parsed.suite().as_slice(),
            NodeWithScopeKind::Function(function) => function.node(&context.parsed).body.as_slice(),
            _ => return Err("the variable's scope is not a function or a module".to_string()),
        };
        if !std::ptr::eq(in_suite.suite, top_level) {
            return Err(format!(
                "`{}` is read by a nested function, and its assignment does not always run",
                assignment.target.id
            ));
        }
    }

    let mut names = Vec::new();
    collect_names(assignment.value, &mut names);
    let in_stub = context.file().is_stub(db);
    for read_name in names {
        let Some(read_scope) = index
            .try_expression_scope_id(&ast::ExprRef::Name(read_name))
            .and_then(|use_scope| resolving_scope(index, use_scope, &read_name.id))
        else {
            // a builtin, or a name nothing binds: nothing in the file rebinds it
            continue;
        };
        let Some(symbol) = index.place_table(read_scope).symbol_id(&read_name.id) else {
            continue;
        };
        if rebound_from_nested_scope(index, read_scope, &read_name.id) {
            return Err(format!(
                "`{}` is rebound by a nested function's `global` or `nonlocal`",
                read_name.id
            ));
        }
        if is_deleted_in_scope(context, index, read_scope, &read_name.id) {
            return Err(format!(
                "`{}` is deleted with `del`, so a read moved past that would find nothing",
                read_name.id
            ));
        }
        let bindings = bindings_of_symbol(db, index, &context.parsed, read_scope, symbol, in_stub);
        if read_scope != scope {
            // the variable's scope reads it from an enclosing one, which may
            // rebind it at any point relative to this code running
            if bindings.len() > 1 {
                return Err(format!(
                    "`{}` is assigned more than once in an enclosing scope",
                    read_name.id
                ));
            }
            continue;
        }
        for binding in bindings {
            let binding_range = binding.full_range(db, &context.parsed).range();
            let after = binding_range.start() >= statement_range.start();
            let in_loop = loops
                .iter()
                .any(|loop_range| loop_range.contains_range(binding_range));
            if after || (in_loop && has_nested_read) {
                return Err(format!(
                    "`{}` may be rebound before `{}` is read",
                    read_name.id, assignment.target.id
                ));
            }
        }
    }
    Ok(())
}

/// Whether anything in `scope` removes `name` with `del`.
///
/// A `del` is not a binding, so it is not among a symbol's definitions — but it takes the name
/// away just as surely as a rebinding replaces it, and a read moved past one finds nothing:
///
/// ```python
/// y = 1
/// x = y
/// del y
/// return x   # inlining `x` here would write `return y`, which raises
/// ```
///
/// Any `del` refuses, wherever it is written. One before the assignment would already have made
/// the assignment itself fail, so there is no position worth telling apart.
fn is_deleted_in_scope(
    context: &RefactorContext<'_>,
    index: &SemanticIndex<'_>,
    scope: FileScopeId,
    name: &Name,
) -> bool {
    struct Deletes<'i, 'db, 'n> {
        index: &'i SemanticIndex<'db>,
        scope: FileScopeId,
        name: &'n Name,
        found: bool,
    }
    impl<'a> SourceOrderVisitor<'a> for Deletes<'_, '_, '_> {
        fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
            if self.found {
                return TraversalSignal::Skip;
            }
            if let AnyNodeRef::ExprName(name) = node
                && name.ctx.is_del()
                && name.id == *self.name
                && self
                    .index
                    .try_expression_scope_id(&ast::ExprRef::Name(name))
                    .and_then(|use_scope| resolving_scope(self.index, use_scope, &name.id))
                    == Some(self.scope)
            {
                self.found = true;
                return TraversalSignal::Skip;
            }
            TraversalSignal::Traverse
        }
    }

    let body = match index.scope(scope).node() {
        NodeWithScopeKind::Module => context.parsed.suite().as_slice(),
        NodeWithScopeKind::Function(function) => function.node(&context.parsed).body.as_slice(),
        // a scope this refactoring does not reach: nothing to say about it
        _ => return false,
    };
    let mut deletes = Deletes {
        index,
        scope,
        name,
        found: false,
    };
    for statement in body {
        walk_node(&mut deletes, AnyNodeRef::from(statement));
    }
    deletes.found
}

fn collect_names<'a>(expr: &'a Expr, names: &mut Vec<&'a ast::ExprName>) {
    struct Names<'a, 'n> {
        names: &'n mut Vec<&'a ast::ExprName>,
    }
    impl<'a> SourceOrderVisitor<'a> for Names<'a, '_> {
        fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
            if let AnyNodeRef::ExprName(name) = node {
                self.names.push(name);
            }
            TraversalSignal::Traverse
        }
    }
    walk_node(&mut Names { names }, AnyNodeRef::from(expr));
}

/// A value with effects is evaluated where it is written. It may move only to a
/// single read, in the statement right after the assignment, with nothing that
/// could observe the difference evaluated before the read.
fn check_single_immediate_read(
    reads: &[Read<'_>],
    scope: FileScopeId,
    in_suite: &super::text::InSuite<'_>,
) -> Result<(), String> {
    let [read] = reads else {
        return Err(
            "its value would be evaluated once for each read, and evaluating it may have effects"
                .to_string(),
        );
    };
    if read.scope != scope {
        return Err(
            "it is read by a nested function, which would evaluate the value when it runs"
                .to_string(),
        );
    }
    let Some(next) = in_suite.next() else {
        return Err("it is not read by the statement right after the assignment".to_string());
    };
    if !next.range().contains_range(read.name.range()) {
        return Err(
            "code runs between the assignment and the read, which would now run before the value is evaluated"
                .to_string(),
        );
    }
    match placement_in_statement(next, AnyNodeRef::from(read.name)) {
        Placement::Once {
            effects_before: false,
        } => Ok(()),
        Placement::Once {
            effects_before: true,
        } => Err(
            "something with effects is evaluated before the read, and would now run before the value"
                .to_string(),
        ),
        Placement::NotOnce(reason) => Err(format!(
            "the read can't take the value's evaluation: {}",
            match reason {
                NotOnce::Body => "it is inside the body of the next statement",
                other => other.describe(),
            }
        )),
    }
}

/// Whether `value`, written in place of `read`, needs parentheses to keep
/// meaning what it means as the assignment's value.
fn needs_parentheses(
    value: &Expr,
    parent: AnyNodeRef<'_>,
    read: &ast::ExprName,
    context: &RefactorContext<'_>,
) -> bool {
    let bracketed = matches!(
        value,
        Expr::List(_)
            | Expr::ListComp(_)
            | Expr::Dict(_)
            | Expr::DictComp(_)
            | Expr::Set(_)
            | Expr::SetComp(_)
    ) || matches!(value, Expr::Tuple(tuple) if tuple.parenthesized)
        || matches!(value, Expr::Generator(generator) if generator.parenthesized);
    if !bracketed && context.is_multiline(value.range()) {
        return true;
    }
    if bracketed {
        return false;
    }
    let atomic = match value {
        Expr::Name(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_)
        | Expr::EllipsisLiteral(_) => true,
        Expr::Call(call) => call.cast_kind.is_none() && !call.is_string_tag,
        Expr::Attribute(attribute) => !attribute.optional,
        Expr::Subscript(subscript) => !subscript.is_typeof && !subscript.is_type_decoration,
        Expr::StringLiteral(string) => !string.value.is_implicit_concatenated(),
        Expr::BytesLiteral(bytes) => !bytes.value.is_implicit_concatenated(),
        Expr::FString(fstring) => !fstring.value.is_implicit_concatenated(),
        _ => false,
    };
    if atomic {
        return false;
    }
    if !is_free_position(parent, read) {
        return true;
    }
    match value {
        Expr::Tuple(_) => !matches!(
            parent,
            AnyNodeRef::StmtExpr(_) | AnyNodeRef::StmtReturn(_) | AnyNodeRef::StmtAssign(_)
        ),
        Expr::Generator(_) => true,
        _ => OperatorPrecedence::from_expr(value) <= OperatorPrecedence::Starred,
    }
}

/// Whether a read is a whole expression on its own — a statement's value, an
/// argument, an element — rather than an operand of something that binds.
fn is_free_position(parent: AnyNodeRef<'_>, read: &ast::ExprName) -> bool {
    let range = read.range();
    match parent {
        AnyNodeRef::StmtExpr(_)
        | AnyNodeRef::StmtReturn(_)
        | AnyNodeRef::StmtAssign(_)
        | AnyNodeRef::StmtAnnAssign(_)
        | AnyNodeRef::StmtAugAssign(_)
        | AnyNodeRef::StmtIf(_)
        | AnyNodeRef::ElifElseClause(_)
        | AnyNodeRef::StmtWhile(_)
        | AnyNodeRef::StmtFor(_)
        | AnyNodeRef::StmtMatch(_)
        | AnyNodeRef::StmtRaise(_)
        | AnyNodeRef::WithItem(_)
        | AnyNodeRef::Keyword(_)
        | AnyNodeRef::Arguments(_)
        | AnyNodeRef::ExprList(_)
        | AnyNodeRef::ExprSet(_)
        | AnyNodeRef::ExprDict(_) => true,
        AnyNodeRef::ExprTuple(tuple) => tuple.parenthesized,
        AnyNodeRef::ExprSubscript(subscript) => subscript.slice.range() == range,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use crate::refactor::RefactorKind;
    use crate::refactor::test_support::RefactorTest;

    fn inline(source: &str) -> String {
        RefactorTest::python(source).apply(RefactorKind::InlineVariable)
    }

    #[test]
    fn replaces_only_reads_the_assignment_reaches() {
        assert_snapshot!(inline(
            r#"
            class Point:
                x = 0

            def other(x=1):
                return x

            def f(obj):
                x<CURSOR> = compute()
                print(x, obj.x, other(x=2), "x")  # x
                return obj

            def compute(): ...
            "#,
        ), @r#"
        Inline variable `x`
        ---

        class Point:
            x = 0

        def other(x=1):
            return x

        def f(obj):
            print(compute(), obj.x, other(x=2), "x")  # x
            return obj

        def compute(): ...
        "#);
    }

    #[test]
    fn value_stops_before_trailing_comment() {
        assert_snapshot!(inline(
            "
            def f():
                x = 1  # note
                print(x<CURSOR>)
            ",
        ), @"
        Inline variable `x`
        ---

        def f():
            print(1)
        ");
    }

    #[test]
    fn pure_value_inlines_into_every_read() {
        assert_snapshot!(inline(
            "
            def f(a):
                x<CURSOR> = a
                print(x)
                return x + x
            ",
        ), @"
        Inline variable `x`
        ---

        def f(a):
            print(a)
            return a + a
        ");
    }

    #[test]
    fn operator_value_is_parenthesized_under_a_tighter_operator() {
        assert_snapshot!(inline(
            "
            def f(a, b):
                x<CURSOR> = a + b
                return x * 2
            ",
        ), @"
        Inline variable `x`
        ---

        def f(a, b):
            return (a + b) * 2
        ");
    }

    #[test]
    fn value_with_effects_is_refused_for_two_reads() {
        assert_snapshot!(inline(
            "
            def f():
                x<CURSOR> = compute()
                print(x)
                print(x)

            def compute(): ...
            ",
        ), @"refused: Inline variable `x` (its value would be evaluated once for each read, and evaluating it may have effects)");
    }

    #[test]
    fn value_with_effects_is_refused_when_code_runs_in_between() {
        assert_snapshot!(inline(
            "
            def f():
                x<CURSOR> = compute()
                log()
                return x

            def compute(): ...
            def log(): ...
            ",
        ), @"refused: Inline variable `x` (code runs between the assignment and the read, which would now run before the value is evaluated)");
    }

    #[test]
    fn value_with_effects_is_refused_after_an_earlier_call_in_the_statement() {
        assert_snapshot!(inline(
            "
            def f():
                x<CURSOR> = compute()
                print(log(), x)

            def compute(): ...
            def log(): ...
            ",
        ), @"refused: Inline variable `x` (something with effects is evaluated before the read, and would now run before the value)");
    }

    #[test]
    fn value_with_effects_is_refused_into_a_loop_condition() {
        assert_snapshot!(inline(
            "
            def f():
                x<CURSOR> = compute()
                while x:
                    pass

            def compute(): ...
            ",
        ), @"refused: Inline variable `x` (the read can't take the value's evaluation: it is evaluated again on every iteration of the loop)");
    }

    #[test]
    fn reassigned_variable_is_refused() {
        assert_snapshot!(inline(
            "
            def f():
                x<CURSOR> = 1
                x = 2
                return x
            ",
        ), @"refused: Inline variable `x` (`x` is assigned more than once)");
    }

    #[test]
    fn augmented_variable_is_refused() {
        assert_snapshot!(inline(
            "
            def f(items):
                total<CURSOR> = 0
                for item in items:
                    total += item
                return total
            ",
        ), @"refused: Inline variable `total` (`total` is assigned more than once)");
    }

    #[test]
    fn single_assignment_is_per_scope() {
        assert_snapshot!(inline(
            "
            def f():
                x<CURSOR> = 1
                return x

            def g():
                x = 2
                return x
            ",
        ), @"
        Inline variable `x`
        ---

        def f():
            return 1

        def g():
            x = 2
            return x
        ");
    }

    #[test]
    fn read_that_may_see_no_value_is_refused() {
        assert_snapshot!(inline(
            "
            def f(flag):
                if flag:
                    x<CURSOR> = 1
                return x
            ",
        ), @"refused: Inline variable `x` (a read of `x` may see another value, or none)");
    }

    #[test]
    fn rebound_operand_is_refused() {
        assert_snapshot!(inline(
            "
            def f(a):
                x<CURSOR> = a
                a = 2
                return x
            ",
        ), @"refused: Inline variable `x` (`a` may be rebound before `x` is read)");
    }

    #[test]
    fn sole_statement_becomes_pass() {
        assert_snapshot!(inline(
            "
            def f(a, lock):
                with lock:
                    x<CURSOR> = a
                return x
            ",
        ), @"
        Inline variable `x`
        ---

        def f(a, lock):
            with lock:
                pass
            return a
        ");
    }

    #[test]
    fn number_is_parenthesized_as_a_receiver() {
        assert_snapshot!(inline(
            "
            def f():
                x<CURSOR> = 1
                return x.real
            ",
        ), @"
        Inline variable `x`
        ---

        def f():
            return (1).real
        ");
    }

    #[test]
    fn module_variable_imported_elsewhere_is_refused() {
        let test = RefactorTest::with_files(
            "main.py",
            "
            X<CURSOR> = 1
            print(X)
            ",
            &[("other.py", "from main import X\n")],
        );
        assert_snapshot!(test.apply(RefactorKind::InlineVariable), @"refused: Inline variable `X` (`X` is imported by another module)");
    }

    #[test]
    fn module_variable_read_by_a_function() {
        assert_snapshot!(inline(
            "
            X<CURSOR> = 1

            def f():
                return X
            ",
        ), @"
        Inline variable `X`
        ---


        def f():
            return 1
        ");
    }

    #[test]
    fn class_attribute_is_refused() {
        assert_snapshot!(inline(
            "
            class A:
                x<CURSOR> = 1
                y = x
            ",
        ), @"refused: Inline variable `x` (`x` is a class attribute, which can be read through the class and its instances)");
    }

    #[test]
    fn parameter_is_not_offered() {
        assert_snapshot!(inline(
            "
            def f(x):
                return x<CURSOR>
            ",
        ), @"not offered");
    }

    #[test]
    fn nested_function_read_of_conditional_assignment_is_refused() {
        assert_snapshot!(inline(
            "
            def f(a, flag):
                if flag:
                    x<CURSOR> = a
                    return lambda: x
            ",
        ), @"refused: Inline variable `x` (`x` is read by a nested function, and its assignment does not always run)");
    }

    #[test]
    fn tuple_value_keeps_its_parentheses_as_an_argument() {
        assert_snapshot!(inline(
            "
            def f(a, b):
                x<CURSOR> = a, b
                print(x)
            ",
        ), @"
        Inline variable `x`
        ---

        def f(a, b):
            print((a, b))
        ");
    }

    #[test]
    fn loop_variable_read_in_the_same_iteration() {
        assert_snapshot!(inline(
            "
            def f(items):
                for item in items:
                    x<CURSOR> = item
                    print(x)
            ",
        ), @"
        Inline variable `x`
        ---

        def f(items):
            for item in items:
                print(item)
        ");
    }

    #[test]
    fn deleted_variable_is_refused() {
        assert_snapshot!(inline(
            "
            def f(a):
                x<CURSOR> = a
                print(x)
                del x
            ",
        ), @"refused: Inline variable `x` (`x` is deleted with `del`)");
    }

    /// The name the value reads is deleted too, and a `del` is not a rebinding — nothing in the
    /// symbol's definitions records it. Inlining here would write `return y` below `del y`, which
    /// raises where the original returned `1`.
    #[test]
    fn deleted_name_in_the_value_is_refused() {
        assert_snapshot!(inline(
            "
            def f():
                y = 1
                <CURSOR>x = y
                del y
                return x
            ",
        ), @"refused: Inline variable `x` (`y` is deleted with `del`, so a read moved past that would find nothing)");
    }

    /// The same when the read is in a nested function, which would call back to a name the
    /// enclosing scope has since given up.
    #[test]
    fn deleted_name_in_the_value_is_refused_for_a_nested_read() {
        assert_snapshot!(inline(
            "
            def f():
                y = 1
                <CURSOR>x = y
                def g():
                    return x
                del y
                return g
            ",
        ), @"refused: Inline variable `x` (`y` is deleted with `del`, so a read moved past that would find nothing)");
    }

    #[test]
    fn read_in_fstring_is_parenthesized() {
        assert_snapshot!(inline(
            r#"
            def f(a, b):
                x<CURSOR> = a
                return f"{x}"
            "#,
        ), @r#"
        Inline variable `x`
        ---

        def f(a, b):
            return f"{(a)}"
        "#);
    }
}
