//! Override and implement members: which methods the class at a position could
//! override, which of them it has to implement, and the declarations that do it,
//! written the way basedpython writes them
//!
//! an editor asks twice: once for the members, to let its user pick
//! ([`members_to_override`]), and once for the declarations of the ones picked
//! ([`override_members`]). both answers come from one walk up the MRO,
//! [`inherited_methods`], and the members are listed with the declaration that
//! would be written for each, so that what is offered is what is written
//!
//! a declaration is the `def` header an override is written with, under the
//! `override` modifier: the method's parameters and return type as the class
//! inherits them, a generic superclass's type parameters filled in the way the
//! class fills them, each type spelled for the file and imported when the file
//! does not reach it already. its body calls the method it overrides, through
//! `super` (a static method, through the superclass's name), or for a method
//! that is abstract where the class inherits it, and so has nothing to call, is
//! `...`
//!
//! a parameter type that cannot be spelled from the file is left out rather
//! than written wrong: a basedpython override's unannotated parameter takes the
//! annotation of the parameter it overrides. a default that is a value is left
//! out of an implementation for the same reason. one that is an expression is
//! not inherited, and is written as its source wrote it, with each name it
//! reads written so that it means what it does where the method is declared

use std::fmt::Write as _;

use itertools::Itertools;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::source_text;
use ruff_diagnostics::Edit;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::source_order::{self, SourceOrderVisitor};
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, Stmt};
use ruff_python_codegen::Stylist;
use ruff_python_trivia::indentation_at_offset;
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};
use ty_module_resolver::file_to_module;
use ty_python_core::ProgramFile;
use ty_python_semantic::ResolvedDefinition;
use ty_python_semantic::types::ide_support::{
    InheritedDefault, InheritedExpression, InheritedMethod, InheritedMethodKind,
    InheritedParameterKind, InheritedSignature, definitions_for_name_in_class_body,
    inherited_methods,
};
use ty_python_semantic::types::{BoundTypeVarInstance, Type};
use ty_python_semantic::{HasType, SemanticModel};

use crate::Db;
use crate::inlay_hints::{AnnotationSpeller, with_annotation_speller};

/// the members the class at a position inherits, for a user to pick from
#[derive(Debug, Clone)]
pub struct ClassMembersToOverride {
    /// the class's name
    pub class_name: Name,
    /// in MRO order of the superclass each is inherited from
    pub members: Vec<MemberToOverride>,
}

/// a method a class could override
#[derive(Debug, Clone)]
pub struct MemberToOverride {
    pub name: Name,
    /// the superclass it is inherited from
    pub superclass: Name,
    /// the module that declares the superclass
    pub superclass_module: Option<String>,
    /// whether it is abstract where it is inherited from, so that the class has
    /// to implement it
    pub is_abstract: bool,
    pub kind: InheritedMethodKind,
    /// the declaration that overrides it, from its name on: `f(self, x: int) -> str`.
    /// an overloaded method's first overload
    pub signature: String,
}

/// the declarations overriding some of a class's members
#[derive(Debug, Clone)]
pub struct MemberOverrides {
    /// the edits that write them, with the imports they need
    pub edits: Vec<Edit>,
    /// the body of the first declaration, in the text the edits leave behind:
    /// what its user replaces first
    pub selection: TextRange,
}

/// the members the class at `offset` could override, in MRO order of the
/// superclass each is inherited from
///
/// the class is the innermost one whose definition holds `offset`: its header,
/// its body, or an indented line after its last statement, which is where a new
/// member would go. `None` when there is no class there, and in a file that is
/// not basedpython source, whose overrides are written another way
pub fn members_to_override(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
) -> Option<ClassMembersToOverride> {
    let context = Context::new(db, file)?;
    let class = context.class_at(offset)?;
    let methods = context.inherited_methods(class);
    let (members, _) = with_annotation_speller(db, file, |speller| {
        let writer = Writer::new(&context, speller, class, offset);
        methods
            .iter()
            .map(|method| MemberToOverride {
                name: method.name.clone(),
                superclass: method.superclass_name.clone(),
                superclass_module: method.superclass_module.as_ref().map(ToString::to_string),
                is_abstract: method.is_abstract,
                kind: method.kind,
                signature: writer.signature(method),
            })
            .collect::<Vec<_>>()
    });
    Some(ClassMembersToOverride {
        class_name: class.name.id.clone(),
        members,
    })
}

/// the declarations overriding the members `names` of the class at `offset`, as
/// [`members_to_override`] lists them, in the order it lists them
///
/// they go where the class's user is: after the statement of the class body
/// that `offset` is in or follows, or at the end of the body when `offset` is in
/// the class's header. a body that is only a placeholder `pass` or `...` is
/// replaced. `None` when there is no class at `offset`, or none of `names` is a
/// member it could override
pub fn override_members(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
    names: &[Name],
) -> Option<MemberOverrides> {
    let context = Context::new(db, file)?;
    let class = context.class_at(offset)?;
    let methods: Vec<_> = context
        .inherited_methods(class)
        .into_iter()
        .filter(|method| names.contains(&method.name))
        .collect();
    if methods.is_empty() {
        return None;
    }
    let place = context.insertion(class, offset);
    let (declarations, imports) = with_annotation_speller(db, file, |speller| {
        let writer = Writer::new(&context, speller, class, place.range.start());
        methods
            .iter()
            .map(|method| writer.declaration(method))
            .collect::<Vec<_>>()
    });

    let line_ending = context.stylist.line_ending().as_str();
    let mut text = place.leading.clone();
    let mut selection = None;
    for (index, declaration) in declarations.iter().enumerate() {
        if index > 0 {
            text.push_str(line_ending);
            text.push_str(line_ending);
        }
        for (line_index, line) in declaration.lines.iter().enumerate() {
            if line_index > 0 {
                text.push_str(line_ending);
            }
            if index > 0 || line_index > 0 || place.indent_first_line {
                text.push_str(&place.indent);
            }
            if index == 0 && line_index + 1 == declaration.lines.len() {
                selection = Some(TextRange::at(
                    text.text_len() + declaration.body_offset,
                    declaration.body_len,
                ));
            }
            text.push_str(line);
        }
    }
    text.push_str(&place.trailing);

    // the selection is in the text the edits leave behind, so it moves by what
    // every import written above the declarations adds
    let (added, removed) = imports
        .iter()
        .filter(|import| import.range.end() <= place.range.start())
        .fold(
            (TextSize::default(), TextSize::default()),
            |(added, removed), import| {
                (
                    added + import.new_text.text_len(),
                    removed + import.range.len(),
                )
            },
        );
    let selection = selection?;
    let selection = TextRange::at(
        place.range.start() + selection.start() + added - removed,
        selection.len(),
    );

    let mut edits = vec![Edit::range_replacement(text, place.range)];
    edits.extend(
        imports
            .into_iter()
            .map(|import| Edit::range_replacement(import.new_text, import.range)),
    );
    Some(MemberOverrides { edits, selection })
}

/// what the two questions read about the file they are asked of
struct Context<'db> {
    db: &'db dyn Db,
    model: SemanticModel<'db>,
    parsed: ParsedModuleRef,
    source: ruff_db::source::SourceText,
    stylist: Stylist<'static>,
}

impl<'db> Context<'db> {
    fn new(db: &'db dyn Db, file: ProgramFile<'db>) -> Option<Self> {
        if !file.file(db).source_type(db).is_basedpython() {
            return None;
        }
        let parsed = parsed_module(db, file.python_file(db)).load(db);
        let source = source_text(db, file.file(db));
        let stylist = Stylist::from_tokens(parsed.tokens(), source.as_str()).into_owned();
        Some(Self {
            db,
            model: SemanticModel::new(db, file),
            parsed,
            source,
            stylist,
        })
    }

    fn source(&self) -> &str {
        self.source.as_str()
    }

    /// the innermost class whose definition holds `offset`, or which `offset`
    /// follows on an indented line with nothing between them but blank lines and
    /// comments: the class body an editor's caret is in when it sits below the
    /// last member, where python's own syntax tree has already ended the class
    fn class_at(&self, offset: TextSize) -> Option<&ast::StmtClassDef> {
        let mut classes = Classes::default();
        classes.visit_body(&self.parsed.syntax().body);
        let source = self.source();
        let column = offset - source.line_start(offset);
        classes
            .found
            .into_iter()
            .filter(|class| {
                if class.range().contains_inclusive(offset) {
                    return true;
                }
                if class.end() > offset {
                    return false;
                }
                let between = &source[TextRange::new(class.end(), offset)];
                let only_trivia = between
                    .lines()
                    .all(|line| line.trim().is_empty() || line.trim_start().starts_with('#'));
                let class_column = class.start() - source.line_start(class.start());
                only_trivia && column > class_column
            })
            .max_by_key(Ranged::start)
    }

    fn inherited_methods(&self, class: &ast::StmtClassDef) -> Vec<InheritedMethod<'db>> {
        let Some(Type::ClassLiteral(literal)) = class.inferred_type(&self.model) else {
            return Vec::new();
        };
        inherited_methods(self.db, &self.model.program_environment(), literal)
    }

    /// the type parameters `class` declares
    fn class_type_params(&self, class: &ast::StmtClassDef) -> Vec<BoundTypeVarInstance<'db>> {
        let Some(Type::ClassLiteral(literal)) = class.inferred_type(&self.model) else {
            return Vec::new();
        };
        literal
            .generic_context(self.db)
            .into_iter()
            .flat_map(|generic_context| generic_context.variables(self.db))
            .collect()
    }

    /// where the declarations go in `class`, for a caret at `offset`
    fn insertion(&self, class: &ast::StmtClassDef, offset: TextSize) -> Insertion {
        let source = self.source();
        let line_ending = self.stylist.line_ending().as_str();
        let indent_unit = self.stylist.indentation().as_str();
        let class_indent = &source[TextRange::new(source.line_start(class.start()), class.start())];

        // a class with no body: one being written, whose header ends in a colon with nothing
        // indented after it yet, or a basedpython class declared without a body at all
        let (Some(first), Some(last)) = (class.body.first(), class.body.last()) else {
            let header = source[..class.end().to_usize()].trim_end();
            let after = &source[class.end().to_usize()..];
            let (at, leading) = if header.ends_with(':') {
                (header.text_len(), line_ending.to_string())
            } else if let Some(colon) = after.trim_start().strip_prefix(':') {
                (
                    source.text_len() - colon.text_len(),
                    line_ending.to_string(),
                )
            } else {
                (class.end(), format!(":{line_ending}"))
            };
            return Insertion {
                range: TextRange::empty(at),
                leading,
                indent: format!("{class_indent}{indent_unit}"),
                indent_first_line: true,
                trailing: String::new(),
            };
        };

        if let [only] = class.body.as_slice()
            && is_placeholder(only)
        {
            return match indentation_at_offset(only.start(), source) {
                // `class A(B):` with the placeholder on a line of its own
                Some(indent) => Insertion {
                    range: only.range(),
                    leading: String::new(),
                    indent: indent.to_string(),
                    indent_first_line: false,
                    trailing: String::new(),
                },
                // `class A(B): ...`, whose placeholder is replaced from the colon
                None => {
                    let before = source[..only.start().to_usize()].trim_end();
                    Insertion {
                        range: TextRange::new(before.text_len(), only.end()),
                        leading: line_ending.to_string(),
                        indent: format!("{class_indent}{indent_unit}"),
                        indent_first_line: true,
                        trailing: String::new(),
                    }
                }
            };
        }

        // `class B(A): x = 1`, whose body moves onto a line of its own with the declarations
        // below it
        let Some(indent) = indentation_at_offset(first.start(), source) else {
            let before = source[..first.start().to_usize()].trim_end();
            let end = source.line_end(last.end());
            let indent = format!("{class_indent}{indent_unit}");
            return Insertion {
                range: TextRange::new(before.text_len(), end),
                leading: format!(
                    "{line_ending}{indent}{}{line_ending}{line_ending}",
                    &source[TextRange::new(first.start(), end)]
                ),
                indent,
                indent_first_line: true,
                trailing: String::new(),
            };
        };
        let indent = indent.to_string();
        // the statement the caret is in or follows; in the header, the body's last
        let anchor = class
            .body
            .iter()
            .rev()
            .find(|statement| statement.start() <= offset)
            .filter(|_| offset >= first.start())
            .unwrap_or(last);
        let mut at = source.line_end(anchor.end());
        // a caret below the anchor goes below the comments between them too: they are what
        // the user was writing after the anchor
        let caret_line = source.line_start(offset);
        let mut line = source.full_line_end(at);
        while line < caret_line {
            let end = source.line_end(line);
            if !source[TextRange::new(line, end)].trim().is_empty() {
                at = end;
            }
            line = source.full_line_end(line);
        }
        // a statement right below the anchor is kept a blank line away
        let next_line_is_code = class
            .body
            .iter()
            .any(|statement| statement.start() > anchor.end())
            && !source[TextRange::new(at, source.full_line_end(source.full_line_end(at)))]
                .trim()
                .is_empty();
        Insertion {
            range: TextRange::empty(at),
            leading: format!("{line_ending}{line_ending}"),
            indent,
            indent_first_line: true,
            trailing: if next_line_is_code {
                line_ending.to_string()
            } else {
                String::new()
            },
        }
    }
}

/// a body statement that stands in for no body: `pass` or `...`
fn is_placeholder(statement: &Stmt) -> bool {
    match statement {
        Stmt::Pass(_) => true,
        Stmt::Expr(expression) => matches!(*expression.value, Expr::EllipsisLiteral(_)),
        _ => false,
    }
}

/// where the declarations are written, and what surrounds them there
struct Insertion {
    /// what they replace: empty to insert them
    range: TextRange,
    /// what goes before the first
    leading: String,
    /// what each of their lines starts with
    indent: String,
    /// whether the first line starts with [`Self::indent`] too, which it does
    /// not when it replaces a statement that is indented already
    indent_first_line: bool,
    /// what goes after the last
    trailing: String,
}

#[derive(Default)]
struct Classes<'a> {
    found: Vec<&'a ast::StmtClassDef>,
}

impl<'a> SourceOrderVisitor<'a> for Classes<'a> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        if let Stmt::ClassDef(class) = statement {
            self.found.push(class);
        }
        source_order::walk_stmt(self, statement);
    }

    fn visit_expr(&mut self, _expression: &'a Expr) {
        // a class is a statement, and no statement is inside an expression
    }
}

/// one declaration, a line at a time, indented from where the declaration is
struct Declaration {
    /// the last is its body
    lines: Vec<String>,
    /// where the body is in its line, after its indentation
    body_offset: TextSize,
    body_len: TextSize,
}

/// writes declarations for a position in one class
struct Writer<'w, 'a, 'db> {
    context: &'w Context<'db>,
    speller: std::cell::RefCell<&'w mut AnnotationSpeller<'a, 'db>>,
    class: &'w ast::StmtClassDef,
    scope_node: AnyNodeRef<'w>,
    offset: TextSize,
    /// the type parameters the class declares, which its body names as they are
    class_type_params: Vec<(BoundTypeVarInstance<'db>, Name)>,
    /// one level of indentation, as the file writes it
    indent_unit: &'w str,
}

impl<'w, 'a, 'db> Writer<'w, 'a, 'db> {
    fn new(
        context: &'w Context<'db>,
        speller: &'w mut AnnotationSpeller<'a, 'db>,
        class: &'w ast::StmtClassDef,
        offset: TextSize,
    ) -> Self {
        let db = context.db;
        Self {
            context,
            speller: std::cell::RefCell::new(speller),
            class,
            scope_node: AnyNodeRef::from(class),
            offset,
            class_type_params: context
                .class_type_params(class)
                .into_iter()
                .map(|typevar| (typevar, typevar.name(db).clone()))
                .collect(),
            indent_unit: context.stylist.indentation().as_str(),
        }
    }

    /// `ty` as an annotation, naming the type parameters in `type_params` and
    /// the class's own as they are
    fn spell(
        &self,
        ty: Type<'db>,
        type_params: &[(BoundTypeVarInstance<'db>, Name)],
    ) -> Option<String> {
        let in_scope: Vec<(BoundTypeVarInstance<'db>, Name)> = type_params
            .iter()
            .chain(&self.class_type_params)
            .cloned()
            .collect();
        self.speller
            .borrow_mut()
            .spell(self.scope_node, self.offset, ty, &in_scope)
    }

    /// what [`MemberToOverride::signature`] says
    fn signature(&self, method: &InheritedMethod<'db>) -> String {
        let Some(signature) = method.signatures.first() else {
            return format!("{}()", method.name);
        };
        let mut text = method.name.to_string();
        self.write_signature(&mut text, method, signature, true);
        text
    }

    fn declaration(&self, method: &InheritedMethod<'db>) -> Declaration {
        let mut lines = Vec::new();
        let body = if method.is_overloaded {
            // each overload is a declaration with no body, and the run ends in
            // an implementation that accepts whatever any of them does
            for signature in &method.signatures {
                let mut line = modifiers(method);
                line.push_str(&method.name);
                self.write_signature(&mut line, method, signature, true);
                lines.push(line);
            }
            let receiver = receiver(method, method.signatures.first());
            let mut line = modifiers(method);
            let _ = write!(
                line,
                "{}({}*args, **kwargs):",
                method.name,
                receiver.map(|name| format!("{name}, ")).unwrap_or_default()
            );
            lines.push(line);
            let returns_none = method
                .signatures
                .iter()
                .all(|signature| signature.returns_none);
            self.body(method, "*args, **kwargs", returns_none)
        } else {
            let signature = method.signatures.first();
            let mut line = modifiers(method);
            line.push_str(&method.name);
            if let Some(signature) = signature {
                self.write_signature(&mut line, method, signature, false);
            } else {
                line.push_str("()");
            }
            line.push(':');
            lines.push(line);
            let arguments = signature
                .map(|signature| arguments(method, signature))
                .unwrap_or_default();
            let returns_none = signature.is_some_and(|signature| signature.returns_none);
            self.body(method, &arguments, returns_none)
        };
        let body_len = body.text_len();
        lines.push(format!("{}{body}", self.indent_unit));
        Declaration {
            body_offset: self.indent_unit.text_len(),
            body_len,
            lines,
        }
    }

    /// the type parameters, parameters and return type, after the name. a
    /// `declaration` is a `def` with no body, an overload, which writes every
    /// default it has; an implementation inherits the ones that are values
    fn write_signature(
        &self,
        text: &mut String,
        method: &InheritedMethod<'db>,
        signature: &InheritedSignature<'db>,
        declaration: bool,
    ) {
        // a method's type parameter named like one of the class's would hide it from the
        // annotations that name the class's, so it is written under a name of its own
        let mut type_params: Vec<(BoundTypeVarInstance<'db>, Name)> = Vec::new();
        for param in &signature.type_params {
            let taken = |name: &str| {
                self.class_type_params
                    .iter()
                    .chain(&type_params)
                    .any(|(_, taken)| taken == name)
            };
            let name = if taken(&param.name) {
                // as many names are taken as there are type parameters, so one of that many
                // and one more suffixes is free
                (2..=self.class_type_params.len() + type_params.len() + 2)
                    .map(|suffix| Name::new(format!("{}{suffix}", param.name)))
                    .find(|candidate| !taken(candidate))
                    .unwrap_or_else(|| param.name.clone())
            } else {
                param.name.clone()
            };
            type_params.push((param.typevar, name));
        }
        if !signature.type_params.is_empty() {
            let params = signature
                .type_params
                .iter()
                .zip(&type_params)
                .map(|(param, (_, name))| {
                    let mut written = name.to_string();
                    if let Some(bound) = param.bound
                        && let Some(bound) = self.spell(bound, &type_params)
                    {
                        let _ = write!(written, ": {bound}");
                    } else if !param.constraints.is_empty()
                        && let Some(constraints) = param
                            .constraints
                            .iter()
                            .map(|constraint| self.spell(*constraint, &type_params))
                            .collect::<Option<Vec<_>>>()
                    {
                        let _ = write!(written, ": ({})", constraints.join(", "));
                    }
                    written
                })
                .join(", ");
            let _ = write!(text, "[{params}]");
        }

        let has_receiver = method.kind != InheritedMethodKind::Static;
        let has_variadic = signature
            .parameters
            .iter()
            .any(|parameter| parameter.kind == InheritedParameterKind::Variadic);
        // a receiver only python's positional-only marker covers is called the same either way,
        // so it gets no marker of its own
        let last_positional_only = signature
            .parameters
            .iter()
            .rposition(|parameter| parameter.kind == InheritedParameterKind::PositionalOnly)
            .filter(|&index| !(has_receiver && index == 0));
        let mut parts = Vec::new();
        let mut wrote_star = false;
        for (index, parameter) in signature.parameters.iter().enumerate() {
            if parameter.kind == InheritedParameterKind::KeywordOnly && !has_variadic && !wrote_star
            {
                parts.push("*".to_string());
                wrote_star = true;
            }
            let mut part = match parameter.kind {
                InheritedParameterKind::Variadic => format!("*{}", parameter.name),
                InheritedParameterKind::KeywordVariadic => format!("**{}", parameter.name),
                _ => parameter.name.to_string(),
            };
            // the receiver's type is implicit
            let annotation = (!(has_receiver && index == 0))
                .then_some(parameter.annotation)
                .flatten()
                .and_then(|annotation| self.spell(annotation, &type_params));
            if let Some(annotation) = &annotation {
                let _ = write!(part, ": {annotation}");
            }
            let default = match &parameter.default {
                Some(InheritedDefault::Value(value)) if declaration => Some(value.clone()),
                Some(InheritedDefault::Expression(expression)) => self.expression(expression),
                _ => None,
            };
            if let Some(default) = default {
                // the spacing python style asks for
                let separator = if annotation.is_some() { " = " } else { "=" };
                let _ = write!(part, "{separator}{default}");
            }
            parts.push(part);
            if Some(index) == last_positional_only {
                parts.push("/".to_string());
            }
        }
        let _ = write!(text, "({})", parts.join(", "));

        if let Some(returns) = signature.returns
            && let Some(returns) = self.spell(returns, &type_params)
        {
            let _ = write!(text, " -> {returns}");
        }
    }

    /// `expression` written in the class, each name it reads written so that it
    /// means there what it means where the expression comes from. `None` when a
    /// name cannot be: one bound only in a class or a function
    fn expression(&self, expression: &InheritedExpression<'db>) -> Option<String> {
        let mut text = expression.text.clone();
        for name in expression.names.iter().rev() {
            let here =
                definitions_for_name_in_class_body(&self.context.model, self.class, &name.name);
            let same = here.len() == name.definitions.len()
                && here
                    .iter()
                    .all(|definition| name.definitions.contains(definition));
            if same {
                continue;
            }
            let reference = self.reference(&name.definitions)?;
            text.replace_range(name.range.to_std_range(), &reference);
        }
        Some(text)
    }

    /// what refers to `definitions` from the class, importing it: a module, or
    /// a name another module binds at its top level
    fn reference(&self, definitions: &[ResolvedDefinition<'db>]) -> Option<String> {
        let db = self.context.db;
        let here = self.context.model.program_file();
        let (file, name) = match definitions {
            [ResolvedDefinition::Module(module)] => (*module, None),
            [ResolvedDefinition::Definition(first), ..] => {
                let file = first.program_file(db);
                let name = first.name(db)?;
                let importable = definitions.iter().all(|definition| {
                    matches!(
                        definition,
                        ResolvedDefinition::Definition(definition)
                            if definition.program_file(db) == file
                                && definition.file_scope(db).is_global()
                                && definition.name(db).as_deref() == Some(name.as_str())
                    )
                });
                if !importable {
                    return None;
                }
                (file, Some(name))
            }
            _ => return None,
        };
        // a name of this file means something else here, and importing the file into itself
        // would not reach it
        if file == here {
            return None;
        }
        let module = file_to_module(db, file.resolver_file(db))?;
        Some(self.speller.borrow_mut().reference(
            self.scope_node,
            self.offset,
            module.name(db).as_str(),
            name.as_deref(),
        ))
    }

    /// the body: a call to the method being overridden, passing `arguments` on,
    /// or `...` when there is nothing to call
    ///
    /// an abstract method has no implementation to call. a static method has no
    /// `super` to call it through, since `super` needs the class or instance a
    /// call was made on, so it is called on the superclass by name, or not at
    /// all when the file cannot name it
    fn body(&self, method: &InheritedMethod<'db>, arguments: &str, returns_none: bool) -> String {
        if method.is_abstract {
            return "...".to_string();
        }
        let receiver = if method.kind == InheritedMethodKind::Static {
            match self.spell(method.superclass_instance, &[]) {
                Some(superclass) => superclass,
                None => return "...".to_string(),
            }
        } else {
            "super".to_string()
        };
        let mut call = format!("{receiver}.{}({arguments})", method.name);
        if method.is_async {
            call = format!("await {call}");
        }
        if returns_none {
            call
        } else {
            format!("return {call}")
        }
    }
}

/// what comes before the name: the modifiers and `def`
fn modifiers(method: &InheritedMethod<'_>) -> String {
    let mut text = String::new();
    if method.is_protected {
        text.push_str("protected ");
    }
    text.push_str("override ");
    match method.kind {
        InheritedMethodKind::Instance => {}
        InheritedMethodKind::Class => text.push_str("class "),
        InheritedMethodKind::Static => text.push_str("static "),
    }
    if method.is_async {
        text.push_str("async ");
    }
    text.push_str("def ");
    text
}

/// the name of the parameter the method is called on: `self`, `cls`. `None`
/// for a static method, which is called on nothing
fn receiver<'s>(
    method: &InheritedMethod<'_>,
    signature: Option<&'s InheritedSignature<'_>>,
) -> Option<&'s Name> {
    if method.kind == InheritedMethodKind::Static {
        return None;
    }
    signature?
        .parameters
        .first()
        .map(|parameter| &parameter.name)
}

/// what a call passing `signature`'s parameters on writes in its
/// parentheses
fn arguments(method: &InheritedMethod<'_>, signature: &InheritedSignature<'_>) -> String {
    let skip = usize::from(method.kind != InheritedMethodKind::Static);
    signature
        .parameters
        .iter()
        .skip(skip)
        .map(|parameter| match parameter.kind {
            InheritedParameterKind::PositionalOnly
            | InheritedParameterKind::PositionalOrKeyword => parameter.name.to_string(),
            InheritedParameterKind::Variadic => format!("*{}", parameter.name),
            InheritedParameterKind::KeywordOnly => {
                format!("{name}={name}", name = parameter.name)
            }
            InheritedParameterKind::KeywordVariadic => format!("**{}", parameter.name),
        })
        .join(", ")
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use ruff_python_ast::name::Name;
    use ruff_text_size::Ranged;

    use crate::tests::{CursorTest, cursor_test};

    impl CursorTest {
        /// one line per member: `Superclass.signature`, `(abstract)` when it is
        fn members_to_override(&self) -> String {
            let Some(members) = super::members_to_override(
                &self.db,
                self.program_file(self.cursor.file),
                self.cursor.offset,
            ) else {
                return "None".to_string();
            };
            let mut lines = vec![format!("class {}", members.class_name)];
            lines.extend(members.members.iter().map(|member| {
                format!(
                    "{}.{}{}",
                    member.superclass,
                    member.signature,
                    if member.is_abstract {
                        " (abstract)"
                    } else {
                        ""
                    }
                )
            }));
            lines.join("\n")
        }

        /// the file once the overrides of `names` are written, with the
        /// selection marked `<SELECTION>…</SELECTION>`
        fn override_members(&self, names: &[&str]) -> String {
            let names: Vec<Name> = names.iter().map(|name| Name::new(*name)).collect();
            let Some(overrides) = super::override_members(
                &self.db,
                self.program_file(self.cursor.file),
                self.cursor.offset,
                &names,
            ) else {
                return "None".to_string();
            };
            let source = ruff_db::source::source_text(&self.db, self.cursor.file);
            let mut edits = overrides.edits.clone();
            edits.sort_by_key(|edit| std::cmp::Reverse(edit.start()));
            let mut text = source.as_str().to_string();
            for edit in &edits {
                text.replace_range(
                    edit.range().to_std_range(),
                    edit.content().unwrap_or_default(),
                );
            }
            let start = overrides.selection.start().to_usize();
            let end = overrides.selection.end().to_usize();
            format!(
                "{}<SELECTION>{}</SELECTION>{}",
                &text[..start],
                &text[start..end],
                &text[end..]
            )
        }
    }

    fn basedpython(source: &str) -> CursorTest {
        CursorTest::builder().source("main.by", source).build()
    }

    /// what [`CursorTest::members_to_override`] lists, less `object`'s members,
    /// which every class inherits
    fn without_object(listed: &str) -> String {
        listed
            .lines()
            .filter(|line| !line.starts_with("object."))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn lists_what_a_class_inherits_with_the_declaration_of_each() {
        let test = basedpython(
            "
from abc import ABC, abstractmethod

class A(ABC):
    @abstractmethod
    def f(self, x: int) -> str: ...

    def g(self, a: int = 1, *args: str, k: list[int] = [], **kw: object) -> int:
        return a

class B(A):
    <CURSOR>
",
        );
        assert_snapshot!(test.members_to_override(), @r#"
        class B
        A.f(self, x: int) -> str (abstract)
        A.g(self, a: int = 1, *args: str, k: list[int] = [], **kw: object) -> int
        object.__setattr__(self, name: str, value: Any, /)
        object.__delattr__(self, name: str, /)
        object.__eq__(self, value: object, /) -> bool
        object.__ne__(self, value: object, /) -> bool
        object.__str__(self) -> str
        object.__repr__(self) -> str
        object.__hash__(self) -> int
        object.__format__(self, format_spec: "", /) -> str
        object.__getattribute__(self, name: str, /) -> Any
        object.__sizeof__(self) -> int
        object.__reduce__(self) -> str | (*: Any)
        object.__reduce_ex__(self, protocol: SupportsIndex, /) -> str | (*: Any)
        object.__getstate__(self) -> object
        object.__dir__(self) -> Iterable[str]
        object.__subclasshook__(cls, subclass: type, /) -> bool
        "#);
    }

    /// what the class defines itself, a `final` method, a `private` one and the
    /// constructors are not offered
    #[test]
    fn leaves_out_what_cannot_be_overridden() {
        let test = basedpython(
            "
class A:
    def kept(self): ...
    def mine(self): ...
    final def fixed(self): ...
    private def hidden(self): ...
    init(self): ...

class B(A):
    def mine(self): ...
    <CURSOR>
",
        );
        assert_snapshot!(without_object(&test.members_to_override()), @"
        class B
        A.kept(self)
        ");
    }

    /// a method's nearest declaration decides whether it is still abstract
    #[test]
    fn an_abstract_method_implemented_further_down_is_not_abstract() {
        let test = basedpython(
            "
from typing import Protocol

class Shape(Protocol):
    def area(self) -> float: ...
    def name(self) -> str: ...

class Square(Shape):
    override def area(self) -> float:
        return 1.0

class Small(Square):
    <CURSOR>
",
        );
        assert_snapshot!(without_object(&test.members_to_override()), @"
        class Small
        Square.area(self) -> float
        Shape.name(self) -> str (abstract)
        ");
    }

    #[test]
    fn no_class_at_the_position_is_none() {
        let test = basedpython(
            "
class A:
    def f(self): ...

def g():
    <CURSOR>pass
",
        );
        assert_snapshot!(test.members_to_override(), @"None");
    }

    /// python's overrides are written with decorators, not basedpython's keyword
    #[test]
    fn a_python_file_is_none() {
        let test = cursor_test(
            "
class A:
    def f(self): ...

class B(A):
    <CURSOR>pass
",
        );
        assert_snapshot!(test.members_to_override(), @"None");
    }

    #[test]
    fn writes_an_override_after_the_statement_the_caret_follows() {
        let test = basedpython(
            "
class A:
    def f(self, x: int) -> str:
        return ''

    def g(self) -> None: ...

class B(A):
    y = 1
    <CURSOR>
",
        );
        assert_snapshot!(test.override_members(&["f", "g"]), @"

        class A:
            def f(self, x: int) -> str:
                return ''

            def g(self) -> None: ...

        class B(A):
            y = 1

            override def f(self, x: int) -> str:
                <SELECTION>return super.f(x)</SELECTION>

            override def g(self):
                super.g()
        ");
    }

    /// a caret in the middle of the body puts the declarations there, a blank
    /// line away from the statement below
    #[test]
    fn writes_between_two_statements() {
        let test = basedpython(
            "
class A:
    def f(self): ...

class B(A):
    x = 1<CURSOR>
    y = 2
",
        );
        assert_snapshot!(test.override_members(&["f"]), @"

        class A:
            def f(self): ...

        class B(A):
            x = 1

            override def f(self):
                <SELECTION>super.f()</SELECTION>

            y = 2
        ");
    }

    #[test]
    fn replaces_a_placeholder_body() {
        let test = basedpython(
            "
class A:
    def f(self): ...

class B(A):
    pass<CURSOR>
",
        );
        assert_snapshot!(test.override_members(&["f"]), @"

        class A:
            def f(self): ...

        class B(A):
            override def f(self):
                <SELECTION>super.f()</SELECTION>
        ");
    }

    #[test]
    fn gives_a_bodyless_class_a_body() {
        let test = basedpython(
            "
class A:
    def f(self): ...

class B<CURSOR>(A)
",
        );
        assert_snapshot!(test.override_members(&["f"]), @"

        class A:
            def f(self): ...

        class B(A):
            override def f(self):
                <SELECTION>super.f()</SELECTION>
        ");
    }

    /// an abstract method has nothing to call, so its body is `...`
    #[test]
    fn implements_an_abstract_method() {
        let test = basedpython(
            "
from abc import ABC, abstractmethod

class A(ABC):
    @abstractmethod
    def f(self, x: int) -> str: ...

class B(A):
    <CURSOR>
",
        );
        assert_snapshot!(test.override_members(&["f"]), @"

        from abc import ABC, abstractmethod

        class A(ABC):
            @abstractmethod
            def f(self, x: int) -> str: ...

        class B(A):
            override def f(self, x: int) -> str:
                <SELECTION>...</SELECTION>
        ");
    }

    /// a generic superclass's parameters are filled in the way the class fills
    /// them. `Iterator` and `Overlapping` are names a `.by` file has without
    /// importing them, so nothing is imported for them
    #[test]
    fn writes_types_as_the_class_inherits_them() {
        let test = basedpython(
            "
from collections.abc import Mapping

class M(Mapping[str, int]):
    <CURSOR>
",
        );
        assert_snapshot!(test.override_members(&["__getitem__", "__iter__"]), @"

        from collections.abc import Mapping

        class M(Mapping[str, int]):
            override def __getitem__(self, key: Overlapping[str], /) -> int:
                <SELECTION>...</SELECTION>

            override def __iter__(self) -> Iterator[str]:
                ...
        ");
    }

    /// a type the file does not reach yet is imported, once however many
    /// declarations name it
    #[test]
    fn imports_what_the_declarations_name() {
        let test = CursorTest::builder()
            .source(
                "shapes.by",
                "
from pathlib import Path

class Shape:
    def save(self, to: Path) -> None: ...
    def load(self, source: Path) -> Path:
        return source
",
            )
            .source(
                "main.by",
                "
from shapes import Shape

class Square(Shape):
    <CURSOR>
",
            )
            .build();
        assert_snapshot!(test.override_members(&["save", "load"]), @"
        from pathlib import Path

        from shapes import Shape

        class Square(Shape):
            override def save(self, to: Path):
                <SELECTION>super.save(to)</SELECTION>

            override def load(self, source: Path) -> Path:
                return super.load(source)
        ");
    }

    /// a classmethod, a staticmethod, an `async def` and a `protected` method
    /// each say so again
    #[test]
    fn keeps_how_a_method_is_declared() {
        let test = basedpython(
            "
class A:
    class def make(cls, n: int) -> int:
        return n

    static def helper(n: int) -> int:
        return n

    async def fetch(self, *, retries: int = 3) -> bytes:
        return b''

    protected def step(self) -> None: ...

class B(A):
    <CURSOR>
",
        );
        assert_snapshot!(
            test.override_members(&["make", "helper", "fetch", "step"]),
            @"

        class A:
            class def make(cls, n: int) -> int:
                return n

            static def helper(n: int) -> int:
                return n

            async def fetch(self, *, retries: int = 3) -> bytes:
                return b''

            protected def step(self) -> None: ...

        class B(A):
            override class def make(cls, n: int) -> int:
                <SELECTION>return super.make(n)</SELECTION>

            override static def helper(n: int) -> int:
                return A.helper(n)

            override async def fetch(self, *, retries: int) -> bytes:
                return await super.fetch(retries=retries)

            protected override def step(self):
                super.step()
        "
        );
    }

    /// an `async def` that yields is an async generator: a call to it returns
    /// the generator rather than a coroutine, so the override returns what the
    /// call to the method it overrides does, with nothing to await
    #[test]
    fn overrides_an_async_generator_without_awaiting_it() {
        let test = basedpython(
            "
from collections.abc import AsyncIterator

class A:
    async def stream(self) -> AsyncIterator[int]:
        yield 1

class B(A):
    <CURSOR>
",
        );
        assert_snapshot!(test.override_members(&["stream"]), @"

        from collections.abc import AsyncIterator

        class A:
            async def stream(self) -> AsyncIterator[int]:
                yield 1

        class B(A):
            override def stream(self) -> AsyncIterator[int]:
                <SELECTION>return super.stream()</SELECTION>
        ");
    }

    /// a default expression names what it reads as the file the override is in
    /// reaches it, importing what that file does not
    #[test]
    fn imports_what_a_default_expression_reads() {
        let test = CursorTest::builder()
            .source(
                "shapes.by",
                "
from pathlib import Path as P

def make_config() -> list[int]:
    return []

class Shape:
    def draw(self, config: list[int] = make_config(), root: P = P('.')) -> None: ...
",
            )
            .source(
                "main.by",
                "
from shapes import Shape

class Square(Shape):
    <CURSOR>
",
            )
            .build();
        assert_snapshot!(test.override_members(&["draw"]), @"
        from pathlib import Path

        from shapes import Shape, make_config

        class Square(Shape):
            override def draw(self, config: list[int] = make_config(), root: Path = Path('.')):
                <SELECTION>super.draw(config, root)</SELECTION>
        ");
    }

    /// a name the file binds to something else is not the one the default
    /// reads, which is written through its module instead
    #[test]
    fn a_default_expression_s_name_shadowed_where_it_is_written_is_qualified() {
        let test = CursorTest::builder()
            .source(
                "shapes.by",
                "
def make_config() -> list[int]:
    return []

class Shape:
    def draw(self, config: list[int] = make_config()) -> None: ...
",
            )
            .source(
                "main.by",
                "
from shapes import Shape

def make_config() -> list[int]:
    return [1]

class Square(Shape):
    <CURSOR>
",
            )
            .build();
        assert_snapshot!(test.override_members(&["draw"]), @"

        import shapes
        from shapes import Shape

        def make_config() -> list[int]:
            return [1]

        class Square(Shape):
            override def draw(self, config: list[int] = shapes.make_config()):
                <SELECTION>super.draw(config)</SELECTION>
        ");
    }

    /// a name a `.by` file has without an import is written bare only where
    /// nothing in the file binds it to something else
    #[test]
    fn an_implicit_name_the_file_shadows_is_qualified() {
        let test = CursorTest::builder()
            .source("mylib.by", "class Iterator: ...\n")
            .source(
                "main.by",
                "
from collections.abc import Mapping
from mylib import Iterator

class M(Mapping[str, int]):
    <CURSOR>
",
            )
            .build();
        assert_snapshot!(test.override_members(&["__iter__"]), @"

        import typing
        from collections.abc import Mapping
        from mylib import Iterator

        class M(Mapping[str, int]):
            override def __iter__(self) -> typing.Iterator[str]:
                <SELECTION>...</SELECTION>
        ");
    }

    /// a method's type parameter named like one of the class's is renamed, so
    /// that each annotation keeps naming the one it names
    #[test]
    fn renames_a_method_type_parameter_the_class_s_would_shadow() {
        let test = basedpython(
            "
class A[T]:
    def f[U](self, u: U, t: T) -> T:
        return t

class B[U](A[U]):
    <CURSOR>
",
        );
        assert_snapshot!(test.override_members(&["f"]), @"

        class A[T]:
            def f[U](self, u: U, t: T) -> T:
                return t

        class B[U](A[U]):
            override def f[U2](self, u: U2, t: U) -> U:
                <SELECTION>return super.f(u, t)</SELECTION>
        ");
    }

    /// a body on the class's header line is moved below it before a
    /// declaration goes after it
    #[test]
    fn writes_after_a_body_on_the_header_line() {
        let test = basedpython(
            "
class A:
    def f(self): ...

class B(A): x = 1<CURSOR>
",
        );
        assert_snapshot!(test.override_members(&["f"]), @"

        class A:
            def f(self): ...

        class B(A):
            x = 1

            override def f(self):
                <SELECTION>super.f()</SELECTION>
        ");
    }

    /// a default that is a value is inherited and left out; one that is an
    /// expression is not, and is written as its source wrote it
    #[test]
    fn writes_only_the_defaults_an_override_does_not_inherit() {
        let test = basedpython(
            "
class A:
    def f(self, a: int = 1, b: list[int] = [], /, c: str | None = None) -> None: ...

class B(A):
    <CURSOR>
",
        );
        assert_snapshot!(test.override_members(&["f"]), @"

        class A:
            def f(self, a: int = 1, b: list[int] = [], /, c: str | None = None) -> None: ...

        class B(A):
            override def f(self, a: int, b: list[int] = [], /, c: str | None):
                <SELECTION>super.f(a, b, c)</SELECTION>
        ");
    }

    /// a generic method declares its type parameters again
    #[test]
    fn writes_a_generic_methods_type_parameters() {
        let test = basedpython(
            "
class A:
    def f[T: int](self, t: T) -> T:
        return t

class B(A):
    <CURSOR>
",
        );
        assert_snapshot!(test.override_members(&["f"]), @"

        class A:
            def f[T: int](self, t: T) -> T:
                return t

        class B(A):
            override def f[T: int](self, t: T) -> T:
                <SELECTION>return super.f(t)</SELECTION>
        ");
    }

    /// each overload is a declaration of its own, and the implementation after
    /// them passes on whatever it is called with
    #[test]
    fn writes_an_overloaded_method_as_its_overloads() {
        let test = basedpython(
            "
class D(dict[str, int]):
    <CURSOR>
",
        );
        assert_snapshot!(test.override_members(&["get"]), @"

        class D(dict[str, int]):
            override def get(self, key: Overlapping[str], default: None = None, /) -> int | None
            override def get(self, key: Overlapping[str], default: int, /) -> int
            override def get[Element](self, key: Overlapping[str], default: Element, /) -> int | Element
            override def get(self, *args, **kwargs):
                <SELECTION>return super.get(*args, **kwargs)</SELECTION>
        ");
    }

    /// a caret on an indented line below the last statement is still in the
    /// class, though python's syntax tree ended it on the line above
    #[test]
    fn a_caret_below_the_last_statement_is_in_the_class() {
        let test = basedpython(
            "
class A:
    def f(self): ...

class B(A):
    x = 1

    # more to come
    <CURSOR>

def after(): ...
",
        );
        assert_snapshot!(test.override_members(&["f"]), @"

        class A:
            def f(self): ...

        class B(A):
            x = 1

            # more to come

            override def f(self):
                <SELECTION>super.f()</SELECTION>
            

        def after(): ...
        ");
    }
}
