//! invert a boolean: give a variable, a parameter or a function the opposite meaning, under a name
//! that says so, and keep every use of it meaning what it meant
//!
//! `flag = True` becomes `not_flag = False`, every read of `flag` becomes `not not_flag` — or
//! `not_flag` where it was `not flag` — and every other value it is given is negated. a parameter's
//! default is negated, and so is the value a call passes it by keyword. a function that returns a
//! `bool` returns the negation of each value it returns, and each call of it is negated
//!
//! what is offered follows PyCharm's *Invert Boolean* for a `.py`: a variable assigned a `True` or
//! a `False`, and a parameter that defaults to one. a function declared to return `bool` is offered
//! too. the new name is `not_` before the old one, or the old one without it, and is the rewrite's
//! made-up name, for the user to rename in place
//!
//! it is refused wherever a use could not be kept meaning what it meant: a binding whose value is
//! not one expression of its own (an augmented assignment, an unpacking, a loop's target), a value
//! that is not a `bool`, whose negation keeps its truth and loses it, a write to an attribute, an
//! attribute of a class, or a function or parameter of a method, whose overrides would have to
//! change with it, a parameter a call may pass by position, which no reference to the parameter
//! shows, a function used other than by calling it, an `async` function, whose call is a coroutine
//! rather than the `bool` it returns, a function defined more than once, each with returns and
//! parameters of its own, an import under another name, whose reads no reference shows, a
//! declaration annotated as other than `bool`, a value written as a statement, which `not` can't
//! go in front of, a name `__all__` lists as a string, and a replacement field with `=`, which
//! prints the name

use ruff_db::files::{File, FileRange};
use ruff_diagnostics::Edit;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::helpers::any_over_expr;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, TraversalSignal, walk_stmt};
use ruff_python_ast::{
    self as ast, AnyNodeRef, Expr, ExprContext, ExprRef, OperatorPrecedence, Stmt, UnaryOp,
};
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::FxHashSet;
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::{ProgramFile, semantic_index};
use ty_python_semantic::ResolvedDefinition;
use ty_python_semantic::types::ide_support::{annotates_bool, dunder_all_lists, is_bool_value};

use super::intentions::{Written, lost_comment, negated};
use super::names::{fresh_name, names_in_file};
use super::{Plan, PlannedName, RefactorContext, Refusal};
use crate::FileEdit;
use crate::goto::{ClassCallName, find_goto_target};
use crate::references::ReferencesMode;
use crate::rename::rename;

/// what is inverted
enum Inverted<'a> {
    Variable,
    Parameter { function: &'a ast::StmtFunctionDef },
    Function(&'a ast::StmtFunctionDef),
}

pub(super) fn plan(context: &RefactorContext<'_>, range: TextRange) -> Result<Plan, Refusal> {
    let db = context.db;
    let Some(goto_target) = find_goto_target(&context.model, &context.parsed, range.start()) else {
        return Err(Refusal::NotApplicable);
    };
    let Some(name) = goto_target.to_string().map(std::borrow::Cow::into_owned) else {
        return Err(Refusal::NotApplicable);
    };
    // a variable assigned more than once has a definition per assignment: the one asked about
    // is the one the caret is on, or the first, when it is on a use
    let definitions = definitions_at(context, range.start());
    let Some(definition) = definitions
        .iter()
        .find(|definition| {
            definition
                .focus_range(db, &context.parsed)
                .range()
                .contains_inclusive(range.start())
        })
        .or_else(|| definitions.first())
        .copied()
    else {
        return Err(Refusal::NotApplicable);
    };
    let title = format!("Invert boolean `{name}`");
    let module = &context.parsed;
    let inverted = match definition.kind(db) {
        DefinitionKind::Assignment(assignment) if is_boolean_literal(assignment.value(module)) => {
            Inverted::Variable
        }
        DefinitionKind::AnnotatedAssignment(assignment)
            if assignment.value(module).is_some_and(is_boolean_literal) =>
        {
            let annotation = assignment.annotation(module);
            if !annotates_bool(&context.model, annotation) {
                return Err(not_annotated_bool(context, &title, annotation));
            }
            Inverted::Variable
        }
        DefinitionKind::Parameter(_) => {
            let Some(function) = definition
                .scope(db)
                .node(db)
                .as_function()
                .map(|function| function.node(module))
            else {
                return Err(Refusal::NotApplicable);
            };
            let Some(parameter) = function
                .parameters
                .iter_non_variadic_params()
                .find(|parameter| parameter.name().as_str() == name)
            else {
                return Err(Refusal::NotApplicable);
            };
            if !parameter.default.as_deref().is_some_and(is_boolean_literal) {
                return Err(Refusal::NotApplicable);
            }
            if let Some(annotation) = parameter.annotation()
                && !annotates_bool(&context.model, annotation)
            {
                return Err(not_annotated_bool(context, &title, annotation));
            }
            let functions = definitions_at(context, function.name.start());
            if functions
                .iter()
                .any(|definition| is_in_class(db, *definition))
            {
                return Err(Refusal::refused(
                    title,
                    "it is a parameter of a method, which the methods it overrides and that \
                     override it would have to change with",
                ));
            }
            if functions
                .iter()
                .any(|definition| is_bound_more_than_once(context, *definition, &function.name))
            {
                return Err(Refusal::refused(
                    title,
                    format!(
                        "`{}` is defined more than once, each with parameters of its own",
                        function.name
                    ),
                ));
            }
            Inverted::Parameter { function }
        }
        DefinitionKind::Function(function) => {
            let function = function.node(module);
            if !function
                .returns
                .as_deref()
                .is_some_and(|returns| annotates_bool(&context.model, returns))
            {
                return Err(Refusal::NotApplicable);
            }
            if is_in_class(db, definition) {
                return Err(Refusal::refused(
                    title,
                    "it is a method, which the methods it overrides and that override it would \
                     have to change with",
                ));
            }
            if function.is_async {
                return Err(Refusal::refused(
                    title,
                    "it is `async`, so that a call of it is a coroutine rather than the `bool` \
                     it returns",
                ));
            }
            if is_bound_more_than_once(context, definition, &function.name) {
                return Err(Refusal::refused(
                    title,
                    "it is defined more than once, each with returns of its own",
                ));
            }
            Inverted::Function(function)
        }
        _ => return Err(Refusal::NotApplicable),
    };
    if matches!(inverted, Inverted::Variable) && is_in_class(db, definition) {
        return Err(Refusal::refused(
            title,
            "it is an attribute of a class, which a subclass overrides by assigning its own and a \
             dataclass's constructor takes by name, neither of which is a use of it",
        ));
    }

    if definition.scope(db).file_scope_id(db).is_global()
        && dunder_all_lists(db, context.file, &name)
    {
        return Err(Refusal::refused(title, listed_in_dunder_all(&name)));
    }

    // every occurrence the symbol's rename makes, which is every place the rewrite reaches
    let placeholder = "__by_inverted__";
    let Some(renamed) = rename(db, context.file, range.start(), placeholder) else {
        return Err(Refusal::NotApplicable);
    };
    let mut occurrences: Vec<FileRange> = renamed
        .iter()
        .map(crate::ReferenceTarget::file_range)
        .collect();
    occurrences.sort_by_key(|occurrence| (occurrence.file(), occurrence.range().start()));
    let mut files: Vec<File> = occurrences.iter().map(FileRange::file).collect();
    files.dedup();

    let contexts: Vec<(File, RefactorContext<'_>)> = files
        .iter()
        .map(|&file| {
            let in_file = ProgramFile::new(db, file, context.file.program(db));
            (file, RefactorContext::new(db, in_file))
        })
        .collect();
    let mut taken = FxHashSet::default();
    for (_, file_context) in &contexts {
        taken.extend(names_in_file(file_context));
    }
    let base = name
        .strip_prefix("not_")
        .filter(|rest| !rest.is_empty())
        .map_or_else(|| format!("not_{name}"), str::to_owned);
    let new_name = fresh_name(&base, &taken);

    if let Inverted::Parameter { function, .. } = inverted {
        check_calls(context, &title, function, &name)?;
    }

    let mut edits: Vec<FileEdit> = Vec::new();
    for (file, file_context) in &contexts {
        for occurrence in occurrences
            .iter()
            .filter(|occurrence| occurrence.file() == *file)
        {
            edits.extend(
                occurrence_edits(file_context, &inverted, occurrence.range(), &new_name)
                    .map_err(|reason| Refusal::refused(&title, reason))?
                    .into_iter()
                    .map(|edit| FileEdit { file: *file, edit }),
            );
        }
    }
    if let Inverted::Function(function) = inverted {
        for value in returned_values(function).map_err(|reason| Refusal::refused(&title, reason))? {
            edits.push(FileEdit {
                file: context.file(),
                edit: inverted_value(context, value.0, value.1)
                    .map_err(|reason| Refusal::refused(&title, reason))?,
            });
        }
    }

    edits.sort_by_key(|edit| (edit.file, edit.edit.start()));
    if edits
        .windows(2)
        .any(|pair| pair[0].file == pair[1].file && pair[0].edit.end() > pair[1].edit.start())
    {
        return Err(Refusal::refused(
            title,
            "one use of it is inside another, which cannot both be rewritten",
        ));
    }
    let (here, foreign): (Vec<FileEdit>, Vec<FileEdit>) = edits
        .into_iter()
        .partition(|edit| edit.file == context.file());
    let edits: Vec<Edit> = here.into_iter().map(|edit| edit.edit).collect();
    // the made-up name, where the declaration is renamed
    let declared_at = definition.focus_range(db, module).range();
    let name = edits
        .iter()
        .position(|edit| edit.range() == declared_at)
        .map(|edit| PlannedName::at(edit, 0, new_name.len()));
    Ok(Plan {
        title: format!("{title} as `{new_name}`"),
        edits,
        name,
        foreign,
    })
}

/// the definitions in `context`'s file of the symbol at `offset`, in the order they are written
///
/// the declaration is where the rewrite is asked for, as PyCharm asks for it, so one in another
/// file is none of these
fn definitions_at<'db>(context: &RefactorContext<'db>, offset: TextSize) -> Vec<Definition<'db>> {
    let db = context.db;
    let declarations =
        find_goto_target(&context.model, &context.parsed, offset).and_then(|target| {
            target
                .definitions(
                    &context.model,
                    ReferencesMode::Rename.to_import_alias_resolution(),
                )?
                .goto_declaration(&context.model, &target, ClassCallName::Class)
        });
    let mut definitions: Vec<Definition<'db>> = declarations
        .iter()
        .flatten()
        .filter_map(|declaration| match declaration {
            ResolvedDefinition::Definition(definition) if definition.file(db) == context.file() => {
                Some(*definition)
            }
            _ => None,
        })
        .collect();
    definitions
        .sort_by_key(|definition| definition.focus_range(db, &context.parsed).range().start());
    definitions
}

/// the edits that keep the occurrence at `range` meaning what it meant, under `new_name`
fn occurrence_edits(
    context: &RefactorContext<'_>,
    inverted: &Inverted<'_>,
    range: TextRange,
    new_name: &str,
) -> Result<Vec<Edit>, String> {
    let rename = Edit::range_replacement(new_name.to_owned(), range);
    let covering = covering_node(context.parsed.syntax().into(), range);
    let ancestors: Vec<AnyNodeRef<'_>> = covering.ancestors().collect();
    let Some((index, node)) = ancestors
        .iter()
        .enumerate()
        .find(|(_, node)| !matches!(node, AnyNodeRef::Identifier(_)))
    else {
        return Err("a use of it is not one this can rewrite".to_owned());
    };
    let parent = ancestors.get(index + 1).copied();
    // `f"{flag=}"` prints what it is written as
    if ancestors.iter().any(|ancestor| {
        matches!(ancestor, AnyNodeRef::InterpolatedElement(element) if element.debug_text.is_some())
    }) {
        return Err(
            "a replacement field with `=` prints it by name, which a rename would change"
                .to_owned(),
        );
    }

    match node {
        // the declaration of a parameter, whose default is negated
        AnyNodeRef::Parameter(_) => {
            let mut edits = vec![rename];
            if let Some(AnyNodeRef::ParameterWithDefault(parameter)) = parent
                && let Some(default) = parameter.default.as_deref()
            {
                edits.push(inverted_value(context, default, parent)?);
            }
            Ok(edits)
        }
        // a function's own declaration, whose returns are negated separately
        AnyNodeRef::StmtFunctionDef(_) => Ok(vec![rename]),
        // a keyword argument, whose value is negated
        AnyNodeRef::Keyword(keyword) => Ok(vec![
            rename,
            inverted_value(context, &keyword.value, Some(*node))?,
        ]),
        // what is read through an import under another name is no use of this name, so nothing
        // would keep it meaning what it meant
        AnyNodeRef::Alias(ast::Alias {
            asname: Some(asname),
            ..
        }) => Err(format!(
            "it is imported as `{asname}`, and what is read through that name cannot be seen"
        )),
        // an import that passes it on through `__all__` keeps the old name there
        AnyNodeRef::Alias(alias)
            if dunder_all_lists(context.db, context.file, alias.name.as_str()) =>
        {
            Err(listed_in_dunder_all(alias.name.as_str()))
        }
        // an import, a `global` or a `nonlocal` only names it
        AnyNodeRef::Alias(_) | AnyNodeRef::StmtGlobal(_) | AnyNodeRef::StmtNonlocal(_) => {
            Ok(vec![rename])
        }
        AnyNodeRef::ExprName(name) => expression_edits(
            context,
            inverted,
            ExprRef::Name(name),
            name.ctx,
            &ancestors[index + 1..],
            new_name,
            rename,
        ),
        AnyNodeRef::ExprAttribute(attribute) if attribute.attr.range() == range => {
            expression_edits(
                context,
                inverted,
                ExprRef::Attribute(attribute),
                attribute.ctx,
                &ancestors[index + 1..],
                new_name,
                rename,
            )
        }
        _ => Err("a use of it is not one this can rewrite".to_owned()),
    }
}

/// the edits for an occurrence that is the expression `expression`, read or written as `ctx`
fn expression_edits(
    context: &RefactorContext<'_>,
    inverted: &Inverted<'_>,
    expression: ExprRef<'_>,
    ctx: ExprContext,
    ancestors: &[AnyNodeRef<'_>],
    new_name: &str,
    rename: Edit,
) -> Result<Vec<Edit>, String> {
    let parent = ancestors.first().copied();
    // the expression's text with the name renamed in it
    let renamed = || {
        let source = context.source();
        let start = usize::from(expression.start());
        let at = usize::from(rename.start()) - start;
        let end = usize::from(rename.end()) - start;
        let text = &source[expression.range()];
        format!("{}{new_name}{}", &text[..at], &text[end..])
    };
    match ctx {
        ExprContext::Load => {
            let read = match inverted {
                // a function is read by calling it, and the call is what is negated
                Inverted::Function(_) => {
                    let Some(AnyNodeRef::ExprCall(call)) = parent else {
                        return Err(format!(
                            "`{}` is used other than by calling it",
                            &context.source()[expression.range()]
                        ));
                    };
                    if call.func.range() != expression.range() {
                        return Err("it is passed to a call rather than called".to_owned());
                    }
                    let source = context.source();
                    let call_text = format!(
                        "{}{}",
                        renamed(),
                        &source[TextRange::new(call.func.end(), call.end())]
                    );
                    // `not f()` reads the new value as it is
                    if let Some(AnyNodeRef::ExprUnaryOp(unary)) = ancestors.get(1)
                        && unary.op == UnaryOp::Not
                    {
                        keeps_comments(context, unary.range(), call.range())?;
                        return Ok(vec![Edit::range_replacement(call_text, unary.range())]);
                    }
                    return Ok(vec![Edit::range_replacement(
                        Written {
                            text: format!("not {call_text}"),
                            precedence: OperatorPrecedence::Not,
                            kept: Vec::new(),
                        }
                        .in_place_of(
                            context,
                            ExprRef::Call(call),
                            ancestors.get(1).copied(),
                        ),
                        call.range(),
                    )]);
                }
                Inverted::Variable | Inverted::Parameter { .. } => renamed(),
            };
            // `not flag` reads the new value as it is
            if let Some(AnyNodeRef::ExprUnaryOp(unary)) = parent
                && unary.op == UnaryOp::Not
            {
                keeps_comments(context, unary.range(), expression.range())?;
                return Ok(vec![Edit::range_replacement(read, unary.range())]);
            }
            Ok(vec![Edit::range_replacement(
                Written {
                    text: format!("not {read}"),
                    precedence: OperatorPrecedence::Not,
                    kept: Vec::new(),
                }
                .in_place_of(context, expression, parent),
                expression.range(),
            )])
        }
        ExprContext::Store => {
            if !matches!(expression, ExprRef::Name(_)) {
                return Err("it is written as an attribute, whose value this cannot see".to_owned());
            }
            match parent {
                Some(AnyNodeRef::StmtAssign(assign)) if matches!(assign.targets.as_slice(), [target] if target.range() == expression.range()) => {
                    Ok(vec![
                        rename,
                        inverted_value(context, &assign.value, parent)?,
                    ])
                }
                Some(AnyNodeRef::StmtAnnAssign(assign)) => {
                    let mut edits = vec![rename];
                    if let Some(value) = assign.value.as_deref() {
                        edits.push(inverted_value(context, value, parent)?);
                    }
                    Ok(edits)
                }
                _ => Err(format!(
                    "`{}` is also bound other than by assigning it a value, which could not be \
                     negated",
                    &context.source()[expression.range()]
                )),
            }
        }
        ExprContext::Del => Ok(vec![rename]),
        ExprContext::Invalid => Err("a use of it is not one this can rewrite".to_owned()),
    }
}

/// `value`, a child of `parent`, written negated: a `True` as `False` and the other way round, as
/// PyCharm writes them, and anything else as [`negated`] writes it, or why it cannot be
///
/// a negation is the opposite of a value's truth, and is no use where the value itself is read:
/// `flag = None` would read back as `False`, which `flag is None` tells apart
fn inverted_value(
    context: &RefactorContext<'_>,
    value: &Expr,
    parent: Option<AnyNodeRef<'_>>,
) -> Result<Edit, String> {
    if let Expr::BooleanLiteral(literal) = value {
        let flipped = if literal.value { "False" } else { "True" };
        return Ok(Edit::range_replacement(flipped.to_owned(), value.range()));
    }
    if any_over_expr(value, &Expr::is_statement_expr) {
        return Err(
            "a value is a statement written as an expression, which `not` can't be put in front of"
                .to_owned(),
        );
    }
    if !is_bool_value(&context.model, value) {
        return Err(format!(
            "`{}` is not a `bool`, and its negation would not keep its value",
            &context.source()[value.range()]
        ));
    }
    let written = negated(context, value);
    if let Some(line) = lost_comment(context, &[value.range()], &written.kept) {
        return Err(format!("the comment on line {line} would be lost"));
    }
    Ok(Edit::range_replacement(
        written.in_place_of(context, ExprRef::from(value), parent),
        value.range(),
    ))
}

/// refuse replacing `replaced` with the text of `kept` when a comment outside it would be lost
fn keeps_comments(
    context: &RefactorContext<'_>,
    replaced: TextRange,
    kept: TextRange,
) -> Result<(), String> {
    match lost_comment(context, &[replaced], &[kept]) {
        Some(line) => Err(format!("the comment on line {line} would be lost")),
        None => Ok(()),
    }
}

/// the refusal for a declaration annotated as more or less than `bool`
fn not_annotated_bool(context: &RefactorContext<'_>, title: &str, annotation: &Expr) -> Refusal {
    Refusal::refused(
        title,
        format!(
            "it is annotated `{}`, whose values a negation would not keep",
            &context.source()[annotation.range()]
        ),
    )
}

/// the refusal for a name `__all__` holds as a string, which no rename reaches
fn listed_in_dunder_all(name: &str) -> String {
    format!("`__all__` lists `{name}` by name, which a rename would leave behind")
}

fn is_boolean_literal(expr: &Expr) -> bool {
    matches!(expr, Expr::BooleanLiteral(_))
}

/// whether the scope `definition` is in binds `name` anywhere else, whatever the caret is on
fn is_bound_more_than_once(
    context: &RefactorContext<'_>,
    definition: Definition<'_>,
    name: &str,
) -> bool {
    let scope = definition.scope(context.db).file_scope_id(context.db);
    let index = semantic_index(context.db, context.file);
    let Some(symbol) = index.place_table(scope).symbol_id(name) else {
        return false;
    };
    index
        .use_def_map(scope)
        .reachable_symbol_bindings(symbol)
        .filter(|binding| binding.binding.definition().is_some())
        .nth(1)
        .is_some()
}

/// whether `definition` is in a class body: a method or an attribute
fn is_in_class(db: &dyn crate::Db, definition: Definition<'_>) -> bool {
    definition.scope(db).node(db).as_class().is_some()
}

/// refuse a parameter some call may pass by position, which no reference to the parameter shows
///
/// every use of the function has to be a call of it, since one passed somewhere else is called
/// where this cannot see, and no call may pass an argument at the parameter's position or later,
/// nor unpack a sequence into its arguments
fn check_calls(
    context: &RefactorContext<'_>,
    title: &str,
    function: &ast::StmtFunctionDef,
    parameter: &str,
) -> Result<(), Refusal> {
    let refused = |reason: String| Err(Refusal::refused(title, reason));
    let position = function
        .parameters
        .posonlyargs
        .iter()
        .chain(&function.parameters.args)
        .position(|candidate| candidate.parameter.name.as_str() == parameter);
    let uses = rename(
        context.db,
        context.file,
        function.name.start(),
        "__by_called__",
    )
    .unwrap_or_default();
    for used in uses {
        let used = used.file_range();
        let used_context = RefactorContext::new(
            context.db,
            ProgramFile::new(context.db, used.file(), context.file.program(context.db)),
        );
        let covering = covering_node(used_context.parsed.syntax().into(), used.range());
        let mut ancestors = covering
            .ancestors()
            .skip_while(|node| matches!(node, AnyNodeRef::Identifier(_)));
        match (ancestors.next(), ancestors.next()) {
            (Some(AnyNodeRef::StmtFunctionDef(_)), _) => {}
            (Some(AnyNodeRef::ExprName(name)), Some(AnyNodeRef::ExprCall(call)))
                if call.func.range() == name.range() =>
            {
                let arguments = &call.arguments;
                if arguments.args.iter().any(Expr::is_starred_expr)
                    || arguments
                        .keywords
                        .iter()
                        .any(|keyword| keyword.arg.is_none())
                {
                    return refused(format!(
                        "a call of `{}` unpacks its arguments, which may pass `{parameter}`",
                        function.name
                    ));
                }
                if let Some(position) = position
                    && arguments.args.len() > position
                {
                    return refused(format!(
                        "a call of `{}` passes `{parameter}` by position",
                        function.name
                    ));
                }
            }
            _ => {
                return refused(format!(
                    "`{}` is used other than by calling it, so its calls cannot all be seen",
                    function.name
                ));
            }
        }
    }
    Ok(())
}

/// every value `function` returns, each with the `return` it is in, or why one cannot be negated
fn returned_values(
    function: &ast::StmtFunctionDef,
) -> Result<Vec<(&Expr, Option<AnyNodeRef<'_>>)>, String> {
    struct Returns<'a> {
        values: Vec<(&'a Expr, Option<AnyNodeRef<'a>>)>,
        problem: Option<String>,
    }
    impl<'a> SourceOrderVisitor<'a> for Returns<'a> {
        fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
            // a nested function or class returns for itself
            if matches!(
                node,
                AnyNodeRef::StmtFunctionDef(_)
                    | AnyNodeRef::StmtClassDef(_)
                    | AnyNodeRef::ExprLambda(_)
            ) {
                TraversalSignal::Skip
            } else {
                TraversalSignal::Traverse
            }
        }

        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            if let Stmt::Return(statement) = stmt {
                match statement.value.as_deref() {
                    Some(value) => self
                        .values
                        .push((value, Some(AnyNodeRef::StmtReturn(statement)))),
                    None => {
                        self.problem.get_or_insert_with(|| {
                            "a `return` returns no value to negate".to_owned()
                        });
                    }
                }
            }
            walk_stmt(self, stmt);
        }

        fn visit_expr(&mut self, expr: &'a Expr) {
            if matches!(expr, Expr::Yield(_) | Expr::YieldFrom(_)) {
                self.problem
                    .get_or_insert_with(|| "it is a generator, which returns no `bool`".to_owned());
            }
            ruff_python_ast::visitor::source_order::walk_expr(self, expr);
        }
    }
    let mut returns = Returns {
        values: Vec::new(),
        problem: None,
    };
    for statement in &function.body {
        returns.visit_stmt(statement);
    }
    match returns.problem {
        Some(problem) => Err(problem),
        None => Ok(returns.values),
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use ruff_text_size::Ranged;

    use crate::refactor::RefactorKind;
    use crate::refactor::test_support::RefactorTest;

    fn invert(source: &str) -> String {
        RefactorTest::python(source).apply(RefactorKind::InvertBoolean)
    }

    /// PyCharm's `simple` and `negate` cases: a read is negated, and a negated read is the new
    /// value as it is
    #[test]
    fn a_variable_and_its_reads() {
        assert_snapshot!(invert(
            r#"
            def foo():
                va<CURSOR>r = True
                if var or not var:
                    return var == 1
                return not var
            "#,
        ), @"
        Invert boolean `var` as `not_var`
        ---

        def foo():
            not_var = False
            if not not_var or not_var:
                return (not not_var) == 1
            return not_var
        ");
    }

    /// every other value it is given is negated, a literal by flipping it
    #[test]
    fn every_value_it_is_given() {
        assert_snapshot!(invert(
            r#"
            def foo(x: int) -> bool:
                do<CURSOR>ne: bool = False
                if x > 1:
                    done = x > 2
                done = True
                return done
            "#,
        ), @"
        Invert boolean `done` as `not_done`
        ---

        def foo(x: int) -> bool:
            not_done: bool = True
            if x > 1:
                not_done = not (x > 2)
            not_done = False
            return not not_done
        ");
    }

    /// PyCharm's `parameter` case, and the value a call passes it by keyword
    #[test]
    fn a_parameter_and_its_keyword_arguments() {
        assert_snapshot!(invert(
            r#"
            def foo(v<CURSOR>ar=True):
                var1 = True
                return var

            foo(var=False)
            foo()
            "#,
        ), @"
        Invert boolean `var` as `not_var`
        ---

        def foo(not_var=False):
            var1 = True
            return not not_var

        foo(not_var=True)
        foo()
        ");
    }

    /// a call that may pass the parameter by position does not say it does
    #[test]
    fn a_parameter_passed_by_position_is_refused() {
        assert_snapshot!(invert(
            r#"
            def foo(v<CURSOR>ar=True):
                return var

            foo(False)
            "#,
        ), @"refused: Invert boolean `var` (a call of `foo` passes `var` by position)");
    }

    /// a function declared to return `bool` returns the negation, and each call of it is negated
    #[test]
    fn a_function_and_its_calls() {
        assert_snapshot!(invert(
            r#"
            def is_<CURSOR>empty(xs: list[int]) -> bool:
                if xs:
                    return False
                return len(xs) == 0

            print(is_empty([]) and not is_empty([1]))
            "#,
        ), @"
        Invert boolean `is_empty` as `not_is_empty`
        ---

        def not_is_empty(xs: list[int]) -> bool:
            if xs:
                return True
            return not (len(xs) == 0)

        print(not not_is_empty([]) and not_is_empty([1]))
        ");
    }

    /// an unpacking gives it a value that is part of another, which cannot be negated alone
    #[test]
    fn an_unpacking_is_refused() {
        assert_snapshot!(invert(
            r#"
            fla<CURSOR>g = True
            flag, other = False, 1
            "#,
        ), @"refused: Invert boolean `flag` (`flag` is also bound other than by assigning it a value, which could not be negated)");
    }

    /// what is not a boolean is not offered
    #[test]
    fn a_number_is_not_offered() {
        assert_snapshot!(invert(
            r#"
            cou<CURSOR>nt = 1
            "#,
        ), @"not offered");
    }

    /// a name that already says `not` loses it
    #[test]
    fn a_not_name_loses_its_not() {
        assert_snapshot!(invert(
            r#"
            not_re<CURSOR>ady = False
            print(not_ready)
            "#,
        ), @"
        Invert boolean `not_ready` as `ready`
        ---

        ready = True
        print(not ready)
        ");
    }

    /// a call of an `async def` is a coroutine, which is not what the `bool` it returns reads as,
    /// and what becomes of it is often no `await` at all
    #[test]
    fn an_async_function_is_refused() {
        assert_snapshot!(invert(
            r#"
            async def o<CURSOR>k() -> bool:
                return True

            async def main():
                print(await ok())
            "#,
        ), @"refused: Invert boolean `ok` (it is `async`, so that a call of it is a coroutine rather than the `bool` it returns)");
    }

    /// a read through an alias is no use of the name, so no rewrite would reach it
    #[test]
    fn an_import_under_another_name_is_refused() {
        let test = RefactorTest::with_files(
            "/flags.py",
            "DEBU<CURSOR>G = True\n",
            &[("/main.py", "from flags import DEBUG as D\n\nprint(D)\n")],
        );
        assert_snapshot!(test.apply(RefactorKind::InvertBoolean), @"refused: Invert boolean `DEBUG` (it is imported as `D`, and what is read through that name cannot be seen)");
    }

    /// a use inside another has one edit inside another's, in any file
    #[test]
    fn a_use_inside_another_in_another_module_is_refused() {
        let test = RefactorTest::with_files(
            "/lib.py",
            "def <CURSOR>f(x: bool) -> bool:\n    return x\n",
            &[("/main.py", "from lib import f\n\nprint(f(f(True)))\n")],
        );
        assert_snapshot!(test.apply(RefactorKind::InvertBoolean), @"refused: Invert boolean `f` (one use of it is inside another, which cannot both be rewritten)");
    }

    /// `not` can't be put in front of a value written as a statement, whether it is returned or
    /// given to the variable
    #[test]
    fn a_value_written_as_a_statement_is_refused() {
        let invert_by =
            |source: &str| RefactorTest::basedpython(source).apply(RefactorKind::InvertBoolean);
        assert_snapshot!(invert_by(
            r#"
            def i<CURSOR>s_one(a: int) -> bool:
                return match a:
                    case 1: True
                    case _: False
            "#,
        ), @"refused: Invert boolean `is_one` (a value is a statement written as an expression, which `not` can't be put in front of)");
        assert_snapshot!(invert_by(
            r#"
            def f(a: int):
                fl<CURSOR>ag = True
                flag = match a:
                    case 1: True
                    case _: False
                return flag
            "#,
        ), @"refused: Invert boolean `flag` (a value is a statement written as an expression, which `not` can't be put in front of)");
    }

    /// a declaration wider or narrower than `bool` holds values a negation would not keep, or that
    /// the opposite literal would not be one of
    #[test]
    fn a_declaration_annotated_as_other_than_bool_is_refused() {
        assert_snapshot!(invert(
            r#"
            def f(fl<CURSOR>ag: bool | None = True):
                return flag is None
            "#,
        ), @"refused: Invert boolean `flag` (it is annotated `bool | None`, whose values a negation would not keep)");
        assert_snapshot!(invert(
            r#"
            from typing import Literal

            x<CURSOR>: Literal[True] = True
            "#,
        ), @"refused: Invert boolean `x` (it is annotated `Literal[True]`, whose values a negation would not keep)");
    }

    /// a qualifier says nothing about the values, and neither does a string
    #[test]
    fn a_declaration_says_bool_however_it_is_written() {
        assert_snapshot!(invert(
            r#"
            from typing import Final

            x<CURSOR>: Final[bool] = True
            print(x)
            "#,
        ), @"
        Invert boolean `x` as `not_x`
        ---

        from typing import Final

        not_x: Final[bool] = False
        print(not not_x)
        ");
        assert_snapshot!(invert(
            r#"
            def f(fl<CURSOR>ag: "bool" = True):
                return flag
            "#,
        ), @r#"
        Invert boolean `flag` as `not_flag`
        ---

        def f(not_flag: "bool" = False):
            return not not_flag
        "#);
    }

    /// `__all__` holds the name as a string, however it comes to hold it
    #[test]
    fn a_name_listed_in_dunder_all_is_refused() {
        assert_snapshot!(invert(
            r#"
            __all__ = ["flag"]
            fl<CURSOR>ag = True
            "#,
        ), @"refused: Invert boolean `flag` (`__all__` lists `flag` by name, which a rename would leave behind)");
        assert_snapshot!(invert(
            r#"
            __all__ = []
            __all__ += ["flag"]
            fl<CURSOR>ag = True
            "#,
        ), @"refused: Invert boolean `flag` (`__all__` lists `flag` by name, which a rename would leave behind)");
    }

    /// a local of the same name is not what `__all__` lists
    #[test]
    fn a_local_is_not_what_dunder_all_lists() {
        assert_snapshot!(invert(
            r#"
            __all__ = ["flag"]
            flag = 1

            def f():
                fl<CURSOR>ag = True
                return flag
            "#,
        ), @r#"
        Invert boolean `flag` as `not_flag`
        ---

        __all__ = ["flag"]
        flag = 1

        def f():
            not_flag = False
            return not not_flag
        "#);
    }

    /// a module that passes the name on through its own `__all__` would keep the old one there
    #[test]
    fn a_name_another_module_lists_in_dunder_all_is_refused() {
        let test = RefactorTest::with_files(
            "/flags.py",
            "DEBU<CURSOR>G = True\n",
            &[(
                "/main.py",
                "from flags import DEBUG\n\n__all__ = [\"DEBUG\"]\n",
            )],
        );
        assert_snapshot!(test.apply(RefactorKind::InvertBoolean), @"refused: Invert boolean `DEBUG` (`__all__` lists `DEBUG` by name, which a rename would leave behind)");
    }

    /// `f"{flag=}"` prints what it is written as, so any rename would change what it prints
    #[test]
    fn a_replacement_field_that_prints_the_name_is_refused() {
        assert_snapshot!(invert(
            r#"
            fl<CURSOR>ag = True
            print(f"{flag=}")
            "#,
        ), @"refused: Invert boolean `flag` (a replacement field with `=` prints it by name, which a rename would change)");
        assert_snapshot!(invert(
            r#"
            def g(fl<CURSOR>ag=True):
                return flag

            print(f"{g(flag=True)=}")
            "#,
        ), @"refused: Invert boolean `flag` (a replacement field with `=` prints it by name, which a rename would change)");
    }

    /// the annotation is what it means, not how it is spelled: an alias of `bool` is `bool`, and a
    /// class of the same name isn't
    #[test]
    fn a_return_annotation_is_read_rather_than_matched_by_name() {
        assert_snapshot!(invert(
            r#"
            Flag = bool

            def i<CURSOR>s_on() -> Flag:
                return True
            "#,
        ), @"
        Invert boolean `is_on` as `not_is_on`
        ---

        Flag = bool

        def not_is_on() -> Flag:
            return False
        ");
        assert_snapshot!(invert(
            r#"
            class bool: ...

            def i<CURSOR>s_on() -> bool:
                return bool()
            "#,
        ), @"not offered");
    }

    /// a subclass overrides an attribute by assigning its own, and a dataclass's constructor takes
    /// it by name, neither of which is a use of the name
    #[test]
    fn an_attribute_of_a_class_is_refused() {
        assert_snapshot!(invert(
            r#"
            class C:
                fl<CURSOR>ag = True

            class D(C):
                flag = False

            print(D().flag)
            "#,
        ), @"refused: Invert boolean `flag` (it is an attribute of a class, which a subclass overrides by assigning its own and a dataclass's constructor takes by name, neither of which is a use of it)");
    }

    /// `not` keeps a value's truth and loses the value, which a read could have been after
    #[test]
    fn a_value_that_is_not_a_bool_is_refused() {
        assert_snapshot!(invert(
            r#"
            fl<CURSOR>ag = True
            flag = None
            print(flag is None)
            "#,
        ), @"refused: Invert boolean `flag` (`None` is not a `bool`, and its negation would not keep its value)");
    }

    /// the same goes for what a call passes a parameter
    #[test]
    fn a_keyword_argument_that_is_not_a_bool_is_refused() {
        assert_snapshot!(invert(
            r#"
            def foo(v<CURSOR>ar=True):
                return var is None

            foo(var=None)
            "#,
        ), @"refused: Invert boolean `var` (`None` is not a `bool`, and its negation would not keep its value)");
    }

    /// a value of unknown type may be a `bool`, as nearly everything in an unannotated module may
    #[test]
    fn a_value_of_unknown_type_is_negated() {
        assert_snapshot!(invert(
            r#"
            from missing import check

            fl<CURSOR>ag = True
            flag = check()
            "#,
        ), @"
        Invert boolean `flag` as `not_flag`
        ---

        from missing import check

        not_flag = False
        not_flag = not check()
        ");
    }

    /// each definition has returns of its own, and only one is under the caret
    #[test]
    fn a_function_defined_more_than_once_is_refused() {
        assert_snapshot!(invert(
            r#"
            import sys

            if sys.argv:
                def <CURSOR>f() -> bool:
                    return True
            else:
                def f() -> bool:
                    return False

            print(f())
            "#,
        ), @"refused: Invert boolean `f` (it is defined more than once, each with returns of its own)");
    }

    /// the same goes for a parameter, which each definition declares for itself
    #[test]
    fn a_parameter_of_a_function_defined_more_than_once_is_refused() {
        assert_snapshot!(invert(
            r#"
            import sys

            if sys.argv:
                def f(v<CURSOR>ar=True):
                    return var
            else:
                def f(var=True):
                    return var
            "#,
        ), @"refused: Invert boolean `var` (`f` is defined more than once, each with parameters of its own)");
    }

    /// a negation rewrites a value from its operands, which a comment between them is not one of
    #[test]
    fn a_comment_a_negated_value_would_lose_is_refused() {
        assert_snapshot!(invert(
            r#"
            def foo(a: bool, b: bool):
                fl<CURSOR>ag = True
                flag = (
                    a  # why
                    and b
                )
                return flag
            "#,
        ), @"refused: Invert boolean `flag` (the comment on line 5 would be lost)");
    }

    /// the uses in another module are rewritten too, and an import of it only renamed
    #[test]
    fn uses_in_another_module() {
        let test = RefactorTest::with_files(
            "/flags.py",
            "DEBU<CURSOR>G = True\n",
            &[(
                "/main.py",
                "from flags import DEBUG\n\nif DEBUG:\n    print(1)\n",
            )],
        );
        let program_file = ty_project::SemanticDb::program_file(&test.db, test.file);
        let result = crate::refactor::refactor(
            &test.db,
            program_file,
            RefactorKind::InvertBoolean,
            test.range,
        )
        .expect("it applies");
        let mut by_file: Vec<String> = result
            .edits
            .iter()
            .map(|edit| {
                format!(
                    "{} {:?} {:?}",
                    edit.file.path(&test.db).as_str().replace('\\', "/"),
                    edit.edit.range(),
                    edit.edit.content().unwrap_or_default()
                )
            })
            .collect();
        by_file.sort();
        assert_snapshot!(by_file.join("\n"), @r#"
        /flags.py 0..5 "not_DEBUG"
        /flags.py 8..12 "False"
        /main.py 18..23 "not_DEBUG"
        /main.py 28..33 "not not_DEBUG"
        "#);
    }
}
