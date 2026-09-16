//! Extract Function / Extract Method: move selected statements into a new
//! function and call it where they were.
//!
//! What the statements read and bind is taken from the semantic index. A
//! variable of the enclosing function the statements read, and that was bound
//! before them, becomes a parameter; a variable they bind that is read after
//! them is returned and assigned at the call. Statements selected in a method
//! become a method of the same class, called through the method's receiver.
//!
//! The rewrite is refused for statements whose meaning depends on being written
//! inline: a `return`, `yield`, `break` or `continue` that leaves the selection,
//! `global` and `nonlocal`, a variable that may be unbound when the new function
//! would be called, or one assigned only on some paths and read afterwards.

use std::fmt::Write as _;

use ruff_diagnostics::Edit;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, TraversalSignal, walk_node};
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, Stmt};
use ruff_python_trivia::{PythonWhitespace, leading_indentation};
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::FxHashSet;
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::place::ScopedPlaceId;
use ty_python_core::scope::{FileScopeId, NodeWithScopeRef};
use ty_python_core::{SemanticIndex, semantic_index};
use ty_python_semantic::HasType;
use ty_python_semantic::types::list_members::all_members;

use super::flow::{reaching_definitions, resolving_scope};
use super::hazards::context_dependent_construct;
use super::names::{fresh_name, names_in_file};
use super::text::{leading_comments_start, nested_suites, statement_ancestors};
use super::{Plan, RefactorContext, Refusal};

pub(super) fn plan(context: &RefactorContext<'_>, range: TextRange) -> Result<Plan, Refusal> {
    let Some(selection) = select_statements(context, range) else {
        return Err(Refusal::NotApplicable);
    };
    let db = context.db;
    let index = semantic_index(db, context.file);
    let source = context.source();
    let module = context.parsed.suite();
    let first = selection.statements[0];
    let last = *selection
        .statements
        .last()
        .expect("a selection has statements");
    let selected = TextRange::new(first.start(), last.end());

    // the definition the statements are written in
    let ancestors = statement_ancestors(module, first.range());
    let owner = ancestors
        .iter()
        .rev()
        .skip(1)
        .find(|stmt| matches!(stmt, Stmt::FunctionDef(_) | Stmt::ClassDef(_)))
        .copied();

    let enclosing = match owner {
        Some(Stmt::ClassDef(_)) => {
            return Err(Refusal::refused(
                "Extract function",
                "statements in a class body define the class's attributes",
            ));
        }
        Some(Stmt::FunctionDef(function)) => Some(function),
        _ => None,
    };
    let scope = match enclosing {
        Some(function) => {
            if function.is_trailing_lambda {
                return Err(Refusal::refused(
                    "Extract function",
                    "the statements are in a trailing lambda block",
                ));
            }
            index
                .try_node_scope(NodeWithScopeRef::Function(function))
                .ok_or(Refusal::NotApplicable)?
        }
        None => FileScopeId::global(),
    };

    let method = match enclosing {
        Some(function) if index.class_definition_of_method(scope).is_some() => {
            Some(Method::of(context, function)?)
        }
        _ => None,
    };
    let class = method.as_ref().and_then(|_| {
        ancestors
            .iter()
            .rev()
            .skip(1)
            .find_map(|stmt| stmt.as_class_def_stmt())
    });

    let mut taken = names_in_file(context);
    if let Some(class) = class
        && let Some(class_type) = class.inferred_type(&context.model)
    {
        let env = context.model.program_environment();
        taken.extend(
            all_members(db, &env, class_type)
                .into_iter()
                .map(|member| member.name.to_string()),
        );
    }
    let name = fresh_name("extracted", &taken);
    let title = if method.is_some() {
        format!("Extract method `{name}`")
    } else {
        format!("Extract function `{name}`")
    };
    let refuse = |reason: String| Refusal::refused(title.clone(), reason);

    // control flow that leaves the statements, and constructs that don't move
    let mut scan = ControlFlowScan::default();
    for statement in &selection.statements {
        scan.visit_stmt(statement);
    }
    if let Some(reason) = scan.refusal {
        return Err(refuse(reason));
    }
    if scan.awaits && !enclosing.is_some_and(|function| function.is_async) {
        return Err(refuse(
            "the statements await outside an `async def`".to_string(),
        ));
    }
    for statement in &selection.statements {
        if let Some(reason) = context_dependent_construct(context, AnyNodeRef::from(*statement)) {
            return Err(refuse(format!("the statements can't be moved: {reason}")));
        }
    }

    let in_selection = |range: TextRange| selected.contains_range(range);
    let table = index.place_table(scope);

    // what the statements bind in the enclosing scope
    let use_def = index.use_def_map(scope);
    let mut bound: Vec<(Name, Definition<'_>)> = Vec::new();
    for (_, definition, _) in use_def.definitions_with_usage() {
        let ScopedPlaceId::Symbol(symbol) = definition.place(db) else {
            continue;
        };
        let kind = definition.kind(db);
        if matches!(
            kind,
            DefinitionKind::LoopHeader(_) | DefinitionKind::NestedBindings(_)
        ) {
            continue;
        }
        if !in_selection(kind.target_range(&context.parsed)) {
            continue;
        }
        let symbol = table.symbol(symbol);
        if symbol.is_global() || symbol.is_nonlocal() {
            return Err(refuse(format!(
                "`{}` is declared `global` or `nonlocal`, so assigning it inside a new function would need the same declaration",
                symbol.name()
            )));
        }
        bound.push((symbol.name().clone(), definition));
    }
    let bound_names: FxHashSet<&Name> = bound.iter().map(|(name, _)| name).collect();

    // what the statements read from the enclosing scope, in the order they read it
    let reads = collect_reads(
        index,
        scope,
        selection.statements.iter().copied(),
        TextRange::empty(TextSize::default()),
    );
    let receiver = method.as_ref().map(|method| &method.receiver);
    let mut parameters: Vec<Name> = Vec::new();
    for read in &reads.names {
        if receiver.is_some_and(|receiver| *receiver == read.name.id) {
            if bound_names.contains(&read.name.id) {
                return Err(refuse(format!(
                    "the statements assign the receiver `{}`",
                    read.name.id
                )));
            }
            continue;
        }
        if read.nested {
            if scope.is_global() {
                if bound_names.contains(&read.name.id) {
                    return Err(refuse(format!(
                        "a function defined in the statements reads `{}`, which would become a variable of the new function",
                        read.name.id
                    )));
                }
                continue;
            }
            let bound_outside = bindings_outside(context, index, scope, &read.name.id, selected);
            if bound_outside && bound_names.contains(&read.name.id) {
                return Err(refuse(format!(
                    "a function defined in the statements reads `{}`, which is assigned both inside and outside them",
                    read.name.id
                )));
            }
            if bound_outside && !parameters.contains(&read.name.id) {
                parameters.push(read.name.id.clone());
            }
            continue;
        }
        let reaching = reaching_definitions(db, index, scope, read.name);
        let from_outside = reaching
            .definitions
            .iter()
            .any(|definition| !in_selection(definition.kind(db).target_range(&context.parsed)));
        if !from_outside {
            continue;
        }
        // a module's variable is read where it is when nothing in the statements
        // makes it local to the new function
        if scope.is_global() && !bound_names.contains(&read.name.id) {
            continue;
        }
        if reaching.maybe_unbound {
            return Err(refuse(format!(
                "`{}` may be unbound where the new function would be called",
                read.name.id
            )));
        }
        if !parameters.contains(&read.name.id) {
            parameters.push(read.name.id.clone());
        }
    }

    // what the statements bind that is read after them
    let mut returned: Vec<Name> = Vec::new();
    let later_reads = match enclosing {
        Some(function) => collect_reads(index, scope, function.body.iter(), selected),
        None => Reads::default(),
    };
    for (name, definition) in &bound {
        if returned.contains(name) {
            continue;
        }
        let read_after = if scope.is_global() {
            // a module's variables are its attributes, read by whoever imports it
            true
        } else {
            later_reads.names.iter().any(|read| {
                read.name.id == *name
                    && (read.nested
                        || reaching_definitions(db, index, scope, read.name)
                            .definitions
                            .iter()
                            .any(|reached| reached == definition))
            })
        };
        if !read_after {
            continue;
        }
        let symbol = table.symbol_id(name).map(|id| table.symbol(id));
        if symbol.is_some_and(ty_python_core::symbol::Symbol::is_declared) {
            return Err(refuse(format!(
                "`{name}` is declared in the statements and read after them, and a declaration can't be returned"
            )));
        }
        if !parameters.contains(name) && !always_binds(&selection.statements, name) {
            return Err(refuse(format!(
                "`{name}` is read after the statements but not assigned on every path through them"
            )));
        }
        returned.push(name.clone());
    }

    // the new definition
    let line_ending = context.line_ending();
    let indent_unit = context.indent_unit();
    let (definition_indent, insert_at, blank_lines) = match enclosing {
        Some(function) => (
            context.indentation(function).to_string(),
            leading_comments_start(source, function.start()),
            if method.is_some() || !scope_is_top_level(index, scope) {
                1
            } else {
                2
            },
        ),
        None => (
            String::new(),
            leading_comments_start(source, ancestors[0].start()),
            2,
        ),
    };
    let body_indent = format!("{definition_indent}{indent_unit}");
    let selection_indent = context.indentation(first).to_string();
    let lines = TextRange::new(
        source.line_start(first.start()),
        source.line_end(last.end()),
    );
    let body = reindent(context, lines, &selection_indent, &body_indent);

    let mut signature = Vec::new();
    if let Some(method) = &method {
        signature.push(method.receiver.to_string());
    }
    signature.extend(parameters.iter().map(ToString::to_string));
    let mut definition = String::new();
    if matches!(
        method,
        Some(Method {
            kind: MethodKind::Class,
            ..
        })
    ) {
        let _ = write!(definition, "{definition_indent}@classmethod{line_ending}");
    }
    let _ = write!(
        definition,
        "{definition_indent}{async_}def {name}({signature}):{line_ending}{body}{line_ending}",
        async_ = if scan.awaits { "async " } else { "" },
        signature = signature.join(", "),
    );
    if !returned.is_empty() {
        let _ = write!(
            definition,
            "{body_indent}return {}{line_ending}",
            join_names(&returned)
        );
    }
    for _ in 0..blank_lines {
        definition.push_str(line_ending);
    }

    // the call
    let callee = match &method {
        Some(method) => format!("{}.{name}", method.receiver),
        None => name,
    };
    let call = format!(
        "{await_}{callee}({arguments})",
        await_ = if scan.awaits { "await " } else { "" },
        arguments = parameters
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    );
    let call_statement = if returned.is_empty() {
        format!("{selection_indent}{call}")
    } else {
        format!("{selection_indent}{} = {call}", join_names(&returned))
    };

    // a definition written directly above the statements it replaces is one edit,
    // since two edits starting at one offset have no order a client must keep
    let edits = if insert_at == lines.start() {
        vec![Edit::range_replacement(
            format!("{definition}{call_statement}"),
            lines,
        )]
    } else {
        vec![
            Edit::insertion(definition, insert_at),
            Edit::range_replacement(call_statement, lines),
        ]
    };
    Ok(Plan { title, edits })
}

/// Whole statements, siblings in one suite, that the selection covers.
struct Selection<'a> {
    statements: Vec<&'a Stmt>,
}

fn select_statements<'a>(
    context: &'a RefactorContext<'_>,
    range: TextRange,
) -> Option<Selection<'a>> {
    let source = context.source();
    let text = source.get(range.to_std_range())?;
    let trimmed = text.trim_whitespace();
    if trimmed.is_empty() {
        return None;
    }
    let start =
        range.start() + TextSize::try_from(text.len() - text.trim_whitespace_start().len()).ok()?;
    let range = TextRange::at(start, TextSize::try_from(trimmed.len()).ok()?);

    let mut suite: &[Stmt] = context.parsed.suite();
    loop {
        let overlapping: Vec<&Stmt> = suite
            .iter()
            .filter(|stmt| {
                stmt.range()
                    .intersect(range)
                    .is_some_and(|overlap| !overlap.is_empty())
                    || range.contains_range(stmt.range())
            })
            .collect();
        match overlapping.as_slice() {
            [] => return None,
            [only] if !covers(source, range, only, only) => {
                // the selection is inside one statement: look in its suites
                let nested = nested_suites(only).into_iter().find(|nested| {
                    nested
                        .iter()
                        .any(|stmt| stmt.range().intersect(range).is_some())
                })?;
                suite = nested;
            }
            statements => {
                let (first, last) = (statements[0], statements[statements.len() - 1]);
                if !covers(source, range, first, last) {
                    return None;
                }
                if !overlapping.iter().all(|stmt| context.owns_its_lines(stmt)) {
                    return None;
                }
                return Some(Selection {
                    statements: overlapping,
                });
            }
        }
    }
}

/// Whether `range` starts at `first` and ends at `last`, allowing the rest of
/// `last`'s line (a trailing comment) to be selected too.
fn covers(source: &str, range: TextRange, first: &Stmt, last: &Stmt) -> bool {
    range.start() <= first.start()
        && source.line_start(range.start()) == source.line_start(first.start())
        && range.end() >= last.end()
        && range.end() <= source.line_end(last.end())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MethodKind {
    Instance,
    Class,
}

struct Method {
    receiver: Name,
    kind: MethodKind,
}

impl Method {
    fn of(context: &RefactorContext<'_>, function: &ast::StmtFunctionDef) -> Result<Self, Refusal> {
        let mut kind = MethodKind::Instance;
        for decorator in &function.decorator_list {
            let Expr::Name(name) = &decorator.expression else {
                continue;
            };
            match name.id.as_str() {
                "staticmethod" | "static" => {
                    return Err(Refusal::refused(
                        "Extract method",
                        "a static method has no receiver to call a new method through",
                    ));
                }
                "classmethod" => kind = MethodKind::Class,
                _ => {}
            }
        }
        let _ = context;
        let receiver = function
            .parameters
            .posonlyargs
            .iter()
            .chain(&function.parameters.args)
            .next()
            .map(|parameter| parameter.parameter.name.id.clone())
            .ok_or_else(|| {
                Refusal::refused("Extract method", "the method has no receiver parameter")
            })?;
        Ok(Method { receiver, kind })
    }
}

/// `return`, `yield`, `break` and `continue` that leave the statements, and
/// statements that only mean something where they are.
#[derive(Default)]
struct ControlFlowScan {
    loop_depth: usize,
    awaits: bool,
    refusal: Option<String>,
}

impl<'a> SourceOrderVisitor<'a> for ControlFlowScan {
    fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
        if self.refusal.is_some() {
            return TraversalSignal::Skip;
        }
        let refusal = match node {
            // a nested scope's control flow is its own
            AnyNodeRef::StmtFunctionDef(_)
            | AnyNodeRef::StmtClassDef(_)
            | AnyNodeRef::ExprLambda(_) => return TraversalSignal::Skip,
            AnyNodeRef::StmtReturn(_) => Some("the statements `return`"),
            AnyNodeRef::ExprYield(_) | AnyNodeRef::ExprYieldFrom(_) => {
                Some("the statements `yield`")
            }
            AnyNodeRef::StmtGlobal(_) | AnyNodeRef::StmtNonlocal(_) => {
                Some("the statements declare `global` or `nonlocal`")
            }
            AnyNodeRef::StmtBreak(stmt) if self.loop_depth == 0 || stmt.value.is_some() => {
                Some("the statements `break` out of a loop around them")
            }
            AnyNodeRef::StmtContinue(_) if self.loop_depth == 0 => {
                Some("the statements `continue` a loop around them")
            }
            AnyNodeRef::StmtLet(_) => {
                Some("a `let` pattern statement leaves its suite when it does not match")
            }
            AnyNodeRef::StmtDelete(_) => Some("the statements delete a variable"),
            AnyNodeRef::ExprAwait(_) => {
                self.awaits = true;
                None
            }
            AnyNodeRef::StmtWith(stmt) => {
                self.awaits |= stmt.is_async;
                None
            }
            AnyNodeRef::Comprehension(comprehension) => {
                self.awaits |= comprehension.is_async;
                None
            }
            // a loop's `break` and `continue` are its body's; the ones in its `else`
            // belong to the loop around it
            AnyNodeRef::StmtFor(stmt) => {
                self.awaits |= stmt.is_async;
                self.visit_expr(&stmt.target);
                self.visit_expr(&stmt.iter);
                self.in_loop(&stmt.body);
                self.visit_body(&stmt.orelse);
                return TraversalSignal::Skip;
            }
            AnyNodeRef::StmtWhile(stmt) => {
                self.visit_expr(&stmt.test);
                self.in_loop(&stmt.body);
                self.visit_body(&stmt.orelse);
                return TraversalSignal::Skip;
            }
            _ => None,
        };
        if let Some(refusal) = refusal {
            self.refusal = Some(refusal.to_string());
            return TraversalSignal::Skip;
        }
        TraversalSignal::Traverse
    }
}

impl ControlFlowScan {
    fn in_loop(&mut self, body: &[Stmt]) {
        self.loop_depth += 1;
        self.visit_body(body);
        self.loop_depth -= 1;
    }
}

struct Read<'a> {
    name: &'a ast::ExprName,
    /// The load is in a scope nested in the enclosing one, so it runs whenever
    /// that scope runs rather than where it is written.
    nested: bool,
}

#[derive(Default)]
struct Reads<'a> {
    names: Vec<Read<'a>>,
}

/// The loads, in `statements` but not in `excluded`, of names that resolve to `scope`.
fn collect_reads<'a>(
    index: &SemanticIndex<'_>,
    scope: FileScopeId,
    statements: impl Iterator<Item = &'a Stmt>,
    excluded: TextRange,
) -> Reads<'a> {
    struct Collector<'a, 'i, 'db> {
        index: &'i SemanticIndex<'db>,
        scope: FileScopeId,
        excluded: TextRange,
        reads: Vec<Read<'a>>,
    }
    impl<'a> SourceOrderVisitor<'a> for Collector<'a, '_, '_> {
        fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
            if !self.excluded.is_empty() && self.excluded.contains_range(node.range()) {
                return TraversalSignal::Skip;
            }
            if let AnyNodeRef::ExprName(name) = node
                && name.ctx.is_load()
                && let Some(use_scope) = self
                    .index
                    .try_expression_scope_id(&ast::ExprRef::Name(name))
                && resolving_scope(self.index, use_scope, &name.id) == Some(self.scope)
            {
                self.reads.push(Read {
                    name,
                    nested: use_scope != self.scope,
                });
            }
            TraversalSignal::Traverse
        }
    }
    let mut collector = Collector {
        index,
        scope,
        excluded,
        reads: Vec::new(),
    };
    for statement in statements {
        walk_node(&mut collector, AnyNodeRef::from(statement));
    }
    Reads {
        names: collector.reads,
    }
}

/// Whether `name` has a binding in `scope` outside `selected`.
fn bindings_outside(
    context: &RefactorContext<'_>,
    index: &SemanticIndex<'_>,
    scope: FileScopeId,
    name: &str,
    selected: TextRange,
) -> bool {
    let db = context.db;
    let Some(symbol) = index.place_table(scope).symbol_id(name) else {
        return false;
    };
    index
        .use_def_map(scope)
        .definitions_with_usage()
        .any(|(_, definition, _)| {
            definition.place(db) == ScopedPlaceId::Symbol(symbol)
                && !matches!(definition.kind(db), DefinitionKind::LoopHeader(_))
                && !selected.contains_range(definition.kind(db).target_range(&context.parsed))
        })
}

/// Whether one of `statements` unconditionally binds `name` to a value.
fn always_binds(statements: &[&Stmt], name: &Name) -> bool {
    statements.iter().any(|statement| match statement {
        Stmt::Assign(assign) => assign
            .targets
            .iter()
            .any(|target| target_binds(target, name)),
        Stmt::AugAssign(assign) => target_binds(&assign.target, name),
        Stmt::AnnAssign(assign) => assign.value.is_some() && target_binds(&assign.target, name),
        Stmt::FunctionDef(function) => function.name.id == *name,
        Stmt::ClassDef(class) => class.name.id == *name,
        Stmt::Import(import) => import.names.iter().any(|alias| {
            alias.asname.as_ref().map_or_else(
                || alias.name.id.split('.').next() == Some(name.as_str()),
                |asname| asname.id == *name,
            )
        }),
        Stmt::ImportFrom(import) => import
            .names
            .iter()
            .any(|alias| alias.asname.as_ref().unwrap_or(&alias.name).id == *name),
        _ => false,
    })
}

fn target_binds(target: &Expr, name: &Name) -> bool {
    match target {
        Expr::Name(target) => target.id == *name,
        Expr::Tuple(tuple) => tuple.elts.iter().any(|element| target_binds(element, name)),
        Expr::List(list) => list.elts.iter().any(|element| target_binds(element, name)),
        Expr::Starred(starred) => target_binds(&starred.value, name),
        _ => false,
    }
}

fn join_names(names: &[Name]) -> String {
    names
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether `scope` is a function defined directly in the module.
fn scope_is_top_level(index: &SemanticIndex<'_>, scope: FileScopeId) -> bool {
    index.scope(scope).parent() == Some(FileScopeId::global())
}

/// The lines of `lines`, moved from `from` indentation to `to`. A line that
/// starts inside a multi-line string is part of the string and is left alone.
fn reindent(context: &RefactorContext<'_>, lines: TextRange, from: &str, to: &str) -> String {
    let source = context.source();
    let tokens = context.parsed.tokens();
    let mut result = String::new();
    let mut offset = lines.start();
    let mut first = true;
    while offset <= lines.end() {
        let line_end = source.line_end(offset);
        let full_end = source.full_line_end(offset).min(lines.end());
        let line = &source[TextRange::new(offset, line_end.min(lines.end()))];
        // tokens are in source order, so the one that could span `offset` is the
        // first that ends after it
        let inside_token = !first
            && tokens
                .get(tokens.partition_point(|token| token.end() <= offset))
                .is_some_and(|token| token.start() < offset);
        if !first {
            result.push_str(context.line_ending());
        }
        first = false;
        if inside_token {
            result.push_str(line);
        } else if line.trim_whitespace().is_empty() {
            // blank lines carry no indentation
        } else {
            let indentation = leading_indentation(line);
            let kept = if indentation.starts_with(from) {
                &line[from.len()..]
            } else {
                &line[indentation.len()..]
            };
            result.push_str(to);
            result.push_str(kept);
        }
        if full_end >= lines.end() {
            break;
        }
        offset = full_end;
    }
    result
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use crate::refactor::RefactorKind;
    use crate::refactor::test_support::RefactorTest;

    fn extract(source: &str) -> String {
        RefactorTest::python(source).apply(RefactorKind::ExtractFunction)
    }

    #[test]
    fn passes_locals_and_returns_what_is_read_after() {
        assert_snapshot!(extract(
            "
            def report(orders, tax):
                <START>total = 0
                for order in orders:
                    total += order.price * tax<END>
                print(total)
            ",
        ), @"
        Extract function `extracted`
        ---

        def extracted(orders, tax):
            total = 0
            for order in orders:
                total += order.price * tax
            return total


        def report(orders, tax):
            total = extracted(orders, tax)
            print(total)
        ");
    }

    #[test]
    fn method_becomes_a_method_called_through_self() {
        assert_snapshot!(extract(
            "
            class Cart:
                def checkout(self, discount):
                    <START>price = self.total() - discount
                    self.charge(price)<END>

                def total(self) -> int: ...
                def charge(self, amount): ...
            ",
        ), @"
        Extract method `extracted`
        ---

        class Cart:
            def extracted(self, discount):
                price = self.total() - discount
                self.charge(price)

            def checkout(self, discount):
                self.extracted(discount)

            def total(self) -> int: ...
            def charge(self, amount): ...
        ");
    }

    #[test]
    fn classmethod_stays_a_classmethod() {
        assert_snapshot!(extract(
            "
            class Config:
                @classmethod
                def load(cls, path):
                    <START>text = open(path).read()<END>
                    return cls(text)
            ",
        ), @"
        Extract method `extracted`
        ---

        class Config:
            @classmethod
            def extracted(cls, path):
                text = open(path).read()
                return text

            @classmethod
            def load(cls, path):
                text = cls.extracted(path)
                return cls(text)
        ");
    }

    #[test]
    fn static_method_is_refused() {
        assert_snapshot!(extract(
            "
            class A:
                @staticmethod
                def f(x):
                    <START>print(x)<END>
            ",
        ), @"refused: Extract method (a static method has no receiver to call a new method through)");
    }

    #[test]
    fn return_is_refused() {
        assert_snapshot!(extract(
            "
            def f(x):
                <START>if x:
                    return 1<END>
                return 2
            ",
        ), @"refused: Extract function `extracted` (the statements `return`)");
    }

    #[test]
    fn break_out_of_an_enclosing_loop_is_refused() {
        assert_snapshot!(extract(
            "
            def f(items):
                for item in items:
                    <START>if item:
                        break<END>
            ",
        ), @"refused: Extract function `extracted` (the statements `break` out of a loop around them)");
    }

    #[test]
    fn break_inside_the_selection_is_allowed() {
        assert_snapshot!(extract(
            "
            def f(items):
                <START>for item in items:
                    if item:
                        break<END>
            ",
        ), @"
        Extract function `extracted`
        ---

        def extracted(items):
            for item in items:
                if item:
                    break


        def f(items):
            extracted(items)
        ");
    }

    #[test]
    fn break_in_a_loop_else_leaves_the_loop_around_it() {
        assert_snapshot!(extract(
            "
            def f(items, groups):
                for group in groups:
                    <START>for item in items:
                        pass
                    else:
                        break<END>
            ",
        ), @"refused: Extract function `extracted` (the statements `break` out of a loop around them)");
    }

    #[test]
    fn variable_assigned_on_some_paths_and_read_after_is_refused() {
        assert_snapshot!(extract(
            "
            def f(flag):
                <START>if flag:
                    x = 1<END>
                print(x)
            ",
        ), @"refused: Extract function `extracted` (`x` is read after the statements but not assigned on every path through them)");
    }

    #[test]
    fn rebound_variable_is_passed_and_returned() {
        assert_snapshot!(extract(
            "
            def f(count):
                <START>count = count + 1<END>
                return count
            ",
        ), @"
        Extract function `extracted`
        ---

        def extracted(count):
            count = count + 1
            return count


        def f(count):
            count = extracted(count)
            return count
        ");
    }

    #[test]
    fn module_level_statements_return_what_they_bind() {
        assert_snapshot!(extract(
            "
            import sys

            <START>name = sys.argv[0]
            print(name)<END>
            ",
        ), @"
        Extract function `extracted`
        ---

        import sys

        def extracted():
            name = sys.argv[0]
            print(name)
            return name


        name = extracted()
        ");
    }

    #[test]
    fn nested_function_goes_inside_the_outer_function() {
        assert_snapshot!(extract(
            "
            def outer(a):
                def inner(b):
                    <START>print(a, b)<END>
                return inner
            ",
        ), @"
        Extract function `extracted`
        ---

        def outer(a):
            def extracted(b):
                print(a, b)

            def inner(b):
                extracted(b)
            return inner
        ");
    }

    #[test]
    fn awaits_make_an_async_function() {
        assert_snapshot!(extract(
            "
            async def f(client):
                <START>response = await client.get()<END>
                return response
            ",
        ), @"
        Extract function `extracted`
        ---

        async def extracted(client):
            response = await client.get()
            return response


        async def f(client):
            response = await extracted(client)
            return response
        ");
    }

    #[test]
    fn multiline_string_is_not_reindented() {
        assert_snapshot!(extract(
            r#"
            def f():
                if True:
                    <START>text = """
            first
              second
            """<END>
                    print(text)
            "#,
        ), @r#"
        Extract function `extracted`
        ---

        def extracted():
            text = """
        first
          second
        """
            return text


        def f():
            if True:
                text = extracted()
                print(text)
        "#);
    }

    #[test]
    fn part_of_a_statement_is_not_offered() {
        assert_snapshot!(extract(
            "
            def f(a):
                print(<START>a<END>)
            ",
        ), @"not offered");
    }

    #[test]
    fn global_declaration_is_refused() {
        assert_snapshot!(extract(
            "
            counter = 0

            def f():
                global counter
                <START>counter = counter + 1<END>
            ",
        ), @"refused: Extract function `extracted` (`counter` is declared `global` or `nonlocal`, so assigning it inside a new function would need the same declaration)");
    }
}
