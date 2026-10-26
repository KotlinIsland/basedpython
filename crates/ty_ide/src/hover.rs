use crate::annotation_expression::is_in_annotation_expression;
use crate::declaration::{Declaration, DeclaredKind, declared_definitions, single_declaration};
use crate::docstring::{Docstring, DocstringFragment};
use crate::goto::{
    Definitions, GotoTarget, docstring_for_call_definition, find_goto_target, find_goto_target_impl,
};
use crate::{Db, MarkupKind, RangedValue};
use ruff_db::files::FileRange;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::source_text;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, AnyNodeRef};
use ruff_python_literal::format::FormatSpec;
use ruff_python_literal::strftime;
use ruff_text_size::{Ranged, TextRange, TextSize};
use std::fmt::{self, Display, Formatter};
use ty_python_core::ProgramFile;
use ty_python_core::definition::DefinitionKind;
use ty_python_semantic::types::ide_support::{
    decorated_function, resolved_call_signature, typed_dict_key_hover,
};
use ty_python_semantic::types::{
    BoundTypeVarInstance, KnownInstanceType, SpecialFormType, Type, TypeVarVariance,
};
use ty_python_semantic::{DisplaySettings, ProgramEnvironment};

use ty_python_semantic::types::format::{SpecLanguage, spec_language};
use ty_python_semantic::{HasType, SemanticModel, TypeQualifiers, with_display_for_file};

pub fn hover<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    offset: TextSize,
) -> Option<RangedValue<Hover<'db>>> {
    // the signature strings a hover carries are rendered here rather than at
    // display time, so they have to be spelled in the file's own syntax now
    with_display_for_file(db, file.file(db), || hover_inner(db, file, offset))
}

fn hover_inner<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    offset: TextSize,
) -> Option<RangedValue<Hover<'db>>> {
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let model = SemanticModel::new(db, file);

    // a format spec is text rather than code, so it has no goto target of its
    // own and has to be recognised before one is looked for
    if let Some(hover) = format_spec_hover(
        db,
        &model.program_environment(),
        &model,
        &parsed,
        file,
        offset,
    ) {
        return Some(hover);
    }

    let goto_target = find_goto_target(&model, &parsed, offset)?;

    if let Some((special_form, range)) = annotation_operator(
        &model,
        parsed.syntax().into(),
        &goto_target,
        offset,
        AnnotationContext::Source,
    ) && let Some(docstring) = Definitions::from_ty(
        db,
        &model.program_environment(),
        Type::SpecialForm(special_form),
    )
    .and_then(|definitions| definitions.docstring(db))
    {
        let ty = Type::SpecialForm(special_form);
        return Some(RangedValue {
            range: FileRange::new(file.file(db), range),
            value: Hover {
                program_file: file,
                type_params_in_scope: Vec::new(),
                contents: vec![
                    HoverContent::Type {
                        ty,
                        variance: None,
                        qualifiers: TypeQualifiers::empty(),
                    },
                    HoverContent::Docstring(docstring),
                ],
            },
        });
    }

    if let GotoTarget::Expression(expr) = goto_target {
        if expr.is_literal_expr() {
            return None;
        }
    }

    let env = model.program_environment();
    let keyword_argument = keyword_argument_hover_contents(db, &model, &goto_target);

    let typed_dict_key = match &goto_target {
        GotoTarget::Expression(ast::ExprRef::Subscript(subscript))
        | GotoTarget::SubscriptStringLiteralKey { subscript, .. } => {
            typed_dict_key_hover(&model, subscript)
        }
        _ => None,
    };

    let mut alias_docstring: Option<Docstring> = None;

    let declared = declared_definitions(&model, &goto_target);
    let declaration = declared
        .as_ref()
        .and_then(|declared| single_declaration(db, declared));
    let decorated = if keyword_argument.is_some() || typed_dict_key.is_some() {
        None
    } else {
        declared.as_ref().and_then(|declared| {
            decorated_function_hover(db, &goto_target, declared, declaration.as_ref())
        })
    };

    let docs = if let Some(decorated) = &decorated {
        decorated.docstring.clone().map(HoverContent::Docstring)
    } else if keyword_argument.is_some() || typed_dict_key.is_some() {
        None
    } else if let GotoTarget::Call { call, .. } = goto_target {
        resolved_call_signature(&model, call)
            .and_then(|details| docstring_for_call_definition(db, details.definition?))
            .or_else(|| {
                // Fall back to the goto-definition targets. This is what
                // surfaces the class docstring for a constructor call like
                // `Foo()`, where the resolved definition is `__init__` and
                // usually carries no docstring of its own.
                goto_target
                    .definitions(
                        &model,
                        ty_python_semantic::ImportAliasResolution::ResolveAliases,
                    )
                    .and_then(|definitions| definitions.docstring(db))
            })
            .map(HoverContent::Docstring)
    } else {
        goto_target
            .definitions(
                &model,
                ty_python_semantic::ImportAliasResolution::ResolveAliases,
            )
            .and_then(|definitions| definitions.docstring(db))
            .map(HoverContent::Docstring)
    };

    let mut contents = Vec::new();
    if keyword_argument.is_none()
        && typed_dict_key.is_none()
        && let Some(declaration) = declaration
    {
        contents.push(HoverContent::Declaration(declaration));
    }
    if let Some(keyword_argument) = keyword_argument {
        contents.extend(keyword_argument);
    } else if let Some(decorated) = decorated {
        if let Some(ty) = decorated.replacement {
            contents.push(HoverContent::Type {
                ty,
                variance: None,
                qualifiers: TypeQualifiers::empty(),
            });
        }
        contents.push(decorated.written);
    } else if let Some(signature) = goto_target.call_signature(&model) {
        contents.push(HoverContent::Signature(signature));
    } else if let Some(typed_dict_key) = typed_dict_key {
        contents.push(HoverContent::TypedDictKey {
            owner: typed_dict_key.owner,
            key: typed_dict_key.key,
            ty: typed_dict_key.declared_ty,
        });
        if let Some(docstring) = typed_dict_key.docstring {
            contents.push(HoverContent::Docstring(Docstring::new(docstring)));
        }
    } else if let Some(ty) = goto_target.inferred_type(&model) {
        tracing::debug!("Inferred type of covering node is {}", ty.display(db, &env));
        let qualifiers = goto_target.type_qualifiers(&model);
        let inferred_type_hover_content = match ty {
            Type::KnownInstance(KnownInstanceType::TypeVar(typevar)) => {
                typevar.bind_pep695(db).map_or(
                    HoverContent::Type {
                        ty,
                        variance: None,
                        qualifiers,
                    },
                    |typevar| HoverContent::Type {
                        ty: Type::TypeVar(typevar),
                        variance: (!typevar.binds_function(db)).then(|| typevar.variance(db)),
                        qualifiers,
                    },
                )
            }
            ty if let Some(declaration) = ty.display_alias_declaration(db, &env) => {
                alias_docstring = Definitions::from_ty(db, &env, ty)
                    .and_then(|def| def.docstring(db))
                    .or_else(|| match ty {
                        Type::KnownInstance(KnownInstanceType::TypeAliasType(alias)) => {
                            Definitions::from_ty(db, &env, alias.value_type(db))
                                .and_then(|def| def.docstring(db))
                        }
                        _ => None,
                    });

                HoverContent::TypeAlias {
                    declaration: declaration.to_string(),
                    qualifiers,
                }
            }
            Type::TypeVar(typevar) => HoverContent::Type {
                ty,
                variance: (!typevar.binds_function(db)).then(|| typevar.variance(db)),
                qualifiers,
            },
            _ => HoverContent::Type {
                ty,
                variance: None,
                qualifiers,
            },
        };
        contents.push(inferred_type_hover_content);
    }

    contents.extend(docs);

    // Aliased docs should come after the docs of the target, if they exist
    if let Some(alias_docstring) = alias_docstring {
        contents.push(HoverContent::Docstring(alias_docstring));
    }

    if contents.is_empty() {
        return None;
    }

    // the semantic index records no scope for a parameter's name, only for the parameter
    let type_params_in_scope = if let GotoTarget::Parameter(parameter) = goto_target {
        model.type_params_in_scope(AnyNodeRef::from(parameter))
    } else {
        covering_node(parsed.syntax().into(), goto_target.range())
            .find_first(|node| node.is_identifier() || node.is_expression())
            .map(|covering| model.type_params_in_scope(covering.node()))
            .unwrap_or_default()
    };

    Some(RangedValue {
        range: FileRange::new(file.file(db), goto_target.range()),
        value: Hover {
            program_file: file,
            contents,
            type_params_in_scope,
        },
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnnotationContext {
    Source,
    StringAnnotation,
}

/// Returns the special form and source range for the operator at `target` if it is `~`, `&`, or `|`
/// in a location recognized by [`is_in_annotation_expression`]. When `target` is a string
/// annotation, `offset` identifies the operator within it.
fn annotation_operator(
    model: &SemanticModel<'_>,
    syntax: ast::AnyNodeRef<'_>,
    target: &GotoTarget<'_>,
    offset: TextSize,
    context: AnnotationContext,
) -> Option<(SpecialFormType, TextRange)> {
    let (special_form, range) = match target {
        GotoTarget::UnaryOp {
            expression,
            operator_range,
        } if expression.op == ast::UnaryOp::Invert => (SpecialFormType::Not, *operator_range),
        GotoTarget::BinOp {
            expression,
            operator_range,
        } => match expression.op {
            ast::Operator::BitAnd => (SpecialFormType::Intersection, *operator_range),
            ast::Operator::BitOr => (SpecialFormType::Union, *operator_range),
            _ => return None,
        },
        GotoTarget::Expression(ast::ExprRef::StringLiteral(string_expr))
        | GotoTarget::StringAnnotationSubexpr { string_expr, .. } => {
            if matches!(context, AnnotationContext::Source)
                && !is_in_annotation_expression(
                    model,
                    &covering_node(syntax, string_expr.range()),
                    string_expr.range(),
                )
            {
                return None;
            }
            let (parsed, submodel) = model.enter_string_annotation(string_expr)?;
            let subtarget =
                find_goto_target_impl(&submodel, parsed.tokens(), parsed.syntax().into(), offset)?;
            return annotation_operator(
                &submodel,
                parsed.syntax().into(),
                &subtarget,
                offset,
                AnnotationContext::StringAnnotation,
            );
        }
        _ => return None,
    };

    if matches!(context, AnnotationContext::StringAnnotation)
        || is_in_annotation_expression(model, &covering_node(syntax, range), range)
    {
        Some((special_form, range))
    } else {
        None
    }
}

/// a decorated `def`: the type its decorators bind the name to, a `wrapper(*args, **kwargs)` or a
/// `_lru_cache_wrapper[...]`, when that is not the function the `def` writes, then the `def` as
/// its source writes it, its decorators over its own signature, and its own docstring rather than
/// the wrapper's
struct DecoratedFunctionHover<'db> {
    replacement: Option<Type<'db>>,
    written: HoverContent<'db>,
    docstring: Option<Docstring>,
}

/// `None` for a `def` without decorators, whose type is the function it writes, and for a use of
/// a property, which reads the property's value rather than naming the `def`.
fn decorated_function_hover<'db>(
    db: &'db dyn Db,
    goto_target: &GotoTarget<'_>,
    declared: &Definitions<'db>,
    declaration: Option<&Declaration>,
) -> Option<DecoratedFunctionHover<'db>> {
    let mut definitions = declared.iter();
    let (Some(ty_python_semantic::ResolvedDefinition::Definition(definition)), None) =
        (definitions.next(), definitions.next())
    else {
        return None;
    };
    let DefinitionKind::Function(function) = definition.kind(db) else {
        return None;
    };
    if !matches!(goto_target, GotoTarget::FunctionDef(_))
        && declaration.is_some_and(|declaration| declaration.kind == DeclaredKind::Property)
    {
        return None;
    }
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let source = source_text(db, definition.file(db));
    let decorators: Vec<String> = function
        .node(&module)
        .decorator_list
        .iter()
        .map(|decorator| format!("@{}", &source[decorator.expression.range()]))
        .collect();
    if decorators.is_empty() {
        return None;
    }
    let function = decorated_function(db, *definition)?;
    let docstring = Definitions::new(vec![ty_python_semantic::ResolvedDefinition::Definition(
        *definition,
    )])
    .docstring(db);
    Some(DecoratedFunctionHover {
        replacement: function.replacement(db),
        written: HoverContent::DecoratedFunction {
            decorators,
            function: function.undecorated,
        },
        docstring,
    })
}

fn keyword_argument_hover_contents<'db>(
    db: &'db dyn Db,
    model: &SemanticModel<'db>,
    goto_target: &GotoTarget<'_>,
) -> Option<Vec<HoverContent<'db>>> {
    let GotoTarget::KeywordArgument {
        keyword,
        call_expression,
    } = goto_target
    else {
        return None;
    };

    let name = keyword.arg.as_ref()?.as_str();
    let signature = resolved_call_signature(model, call_expression)?;
    let argument_index = call_expression
        .arguments
        .iter_source_order()
        .position(|argument| {
            matches!(argument, ast::ArgOrKeyword::Keyword(candidate) if candidate.range == keyword.range)
        })?;
    let argument_mapping = signature
        .argument_to_parameter_mapping
        .get(argument_index)?;
    if !argument_mapping.matched {
        return None;
    }

    let parameter_index = signature
        .argument_to_displayed_parameter_mapping
        .get(argument_index)
        .copied()
        .flatten()?;
    let parameter = signature.parameters.get(parameter_index)?;

    if !parameter.is_keyword_variadic && parameter.name != name {
        return None;
    }

    let mut contents = vec![HoverContent::Parameter(parameter_hover_label(
        &parameter.label,
        parameter.is_keyword_variadic,
    ))];
    if let Some(documentation) = signature
        .definition
        .and_then(|definition| docstring_for_call_definition(db, definition))
        .and_then(|docstring| documentation_for_parameter(&docstring, &parameter.name))
    {
        contents.push(HoverContent::DocstringFragment(documentation));
    }

    Some(contents)
}

fn parameter_hover_label(label: &str, is_keyword_variadic: bool) -> String {
    let kind = if is_keyword_variadic {
        "variadic keyword parameter"
    } else {
        "parameter"
    };

    format!("({kind}) {label}")
}

fn documentation_for_parameter(docstring: &Docstring, name: &str) -> Option<DocstringFragment> {
    let parameter_documentation = docstring.parameter_documentation();
    parameter_documentation
        .get(name)
        .or_else(|| {
            name.strip_prefix("**")
                .and_then(|name| parameter_documentation.get(name))
        })
        .map(|documentation| DocstringFragment::new(documentation))
}

/// hover for an f-string format spec: what each clause does, and what the spec
/// makes of a sample value
fn format_spec_hover<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    model: &SemanticModel<'db>,
    parsed: &ParsedModuleRef,
    file: ProgramFile<'db>,
    offset: TextSize,
) -> Option<RangedValue<Hover<'db>>> {
    let covering = covering_node(parsed.syntax().into(), TextRange::empty(offset));
    // the innermost replacement field: one nested inside a spec has no spec of
    // its own, so hovering in it falls through to ordinary hover
    let field = covering.ancestors().find_map(|node| match node {
        AnyNodeRef::InterpolatedElement(field) => Some(field),
        _ => None,
    })?;
    let spec = field.format_spec.as_deref()?;
    if !spec.range().contains_inclusive(offset) || spec.range().is_empty() {
        return None;
    }

    let source = source_text(db, file.file(db));
    let written = source.get(spec.range().start().to_usize()..spec.range().end().to_usize())?;

    let language = field
        .expression
        .inferred_type(model)
        .and_then(|ty| spec_language(db, env, ty));
    let (sample, clauses) = match language {
        // `date`, `time` and `datetime` read strftime directives, a language
        // the mini-language's clauses say nothing about
        Some(SpecLanguage::Strftime) => (
            Some((strftime::SAMPLE.to_string(), strftime::preview(written))),
            strftime::directives(written)
                .into_iter()
                .map(|directive| {
                    (
                        written[directive.span.clone()].to_string(),
                        directive.documentation().to_string(),
                    )
                })
                .collect(),
        ),
        target => {
            let (FormatSpec::Static(parsed), spans) = FormatSpec::parse_spanned(written).ok()?
            else {
                // a spec assembled at runtime says nothing about its own shape
                return None;
            };
            let target = match target {
                Some(SpecLanguage::MiniLanguage(target)) => Some(target),
                _ => None,
            };
            (
                target.map(|target| (target.sample_source().to_string(), parsed.preview(target))),
                spans
                    .iter()
                    .filter_map(|(component, span)| {
                        let text = written.get(span)?;
                        Some((text.to_string(), component.describe(text, target)))
                    })
                    .collect(),
            )
        }
    };

    let contents = vec![HoverContent::FormatSpec(Box::new(FormatSpecHover {
        written: written.to_string(),
        sample,
        clauses,
    }))];
    Some(RangedValue {
        range: FileRange::new(file.file(db), spec.range()),
        value: Hover {
            program_file: file,
            contents,
            type_params_in_scope: Vec::new(),
        },
    })
}

/// what a format spec does, and what it does to a sample value
#[derive(Debug, Clone)]
pub struct FormatSpecHover {
    /// the spec as written
    written: String,
    /// the sample value this spec was applied to, and the result — `None` when
    /// the spec is one the value's `__format__` rejects
    sample: Option<(String, Option<String>)>,
    /// each written clause, as written, with what it does
    clauses: Vec<(String, String)>,
}

impl fmt::Display for FormatSpecHover {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "format spec `{}`", self.written)?;
        if let Some((sample, rendered)) = &self.sample {
            match rendered {
                Some(rendered) => writeln!(f, "\n`{sample}` formats as `{rendered}`")?,
                None => writeln!(f, "\nthis spec is not one the value accepts")?,
            }
        }
        if !self.clauses.is_empty() {
            // a list, so each clause gets its own line in both markup kinds
            f.write_str("\n")?;
            for (written, description) in &self.clauses {
                writeln!(f, "- `{written}` {description}")?;
            }
        }
        Ok(())
    }
}

pub struct Hover<'db> {
    program_file: ProgramFile<'db>,
    contents: Vec<HoverContent<'db>>,
    /// the type parameters an annotation at the hovered position names by a bare name, which
    /// the hover writes that way too
    type_params_in_scope: Vec<(BoundTypeVarInstance<'db>, Name)>,
}

impl<'db> Hover<'db> {
    /// Renders the hover to a string using the specified markup kind.
    pub const fn display<'a>(&'a self, db: &'db dyn Db, kind: MarkupKind) -> impl Display {
        std::fmt::from_fn(move |f| {
            // a hover shows the type as the reader would write it in this file:
            // `1` in a basedpython file, `Literal[1]` in a python one. the contents
            // render here rather than where the hover was built, so the file's own
            // spelling has to be in force at this point too
            with_display_for_file(db, self.program_file.file(db), || {
                let mut first = true;
                let env = ProgramEnvironment::from_file(self.program_file);
                for content in &self.contents {
                    if !first {
                        kind.horizontal_line().fmt(f)?;
                    }

                    content
                        .display(db, &env, &self.type_params_in_scope, kind)
                        .fmt(f)?;
                    first = false;
                }

                Ok(())
            })
        })
    }

    fn iter(&self) -> std::slice::Iter<'_, HoverContent<'db>> {
        self.contents.iter()
    }
}

impl<'db> IntoIterator for Hover<'db> {
    type Item = HoverContent<'db>;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.contents.into_iter()
    }
}

impl<'a, 'db> IntoIterator for &'a Hover<'db> {
    type Item = &'a HoverContent<'db>;
    type IntoIter = std::slice::Iter<'a, HoverContent<'db>>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[derive(Debug, Clone)]
pub enum HoverContent<'db> {
    /// What the hovered name declares: its kind, and its qualified name when it has one.
    Declaration(Declaration),
    /// A decorated `def`: its decorators, as written, over the function it writes.
    DecoratedFunction {
        decorators: Vec<String>,
        function: Type<'db>,
    },
    Signature(String),
    Parameter(String),
    Type {
        // The type of the target being hovered
        ty: Type<'db>,
        // The type's variance
        variance: Option<TypeVarVariance>,
        // The type's qualifiers
        qualifiers: TypeQualifiers,
    },
    TypeAlias {
        // The declaration of the type alias being hovered
        declaration: String,
        // The type's qualifiers
        qualifiers: TypeQualifiers,
    },
    TypedDictKey {
        owner: String,
        key: String,
        ty: Type<'db>,
    },
    Docstring(Docstring),
    DocstringFragment(DocstringFragment),
    FormatSpec(Box<FormatSpecHover>),
}

impl<'db> HoverContent<'db> {
    fn display<'a>(
        &'a self,
        db: &'db dyn Db,
        env: &'a ProgramEnvironment<'db>,
        type_params_in_scope: &'a [(BoundTypeVarInstance<'db>, Name)],
        kind: MarkupKind,
    ) -> DisplayHoverContent<'a, 'db> {
        DisplayHoverContent {
            db,
            env,
            type_params_in_scope,
            content: self,
            kind,
        }
    }
}

pub(crate) struct DisplayHoverContent<'a, 'db> {
    db: &'db dyn Db,
    env: &'a ProgramEnvironment<'db>,
    type_params_in_scope: &'a [(BoundTypeVarInstance<'db>, Name)],
    content: &'a HoverContent<'db>,
    kind: MarkupKind,
}

impl<'db> DisplayHoverContent<'_, 'db> {
    fn ty_string_and_syntax(&self, ty: &Type<'db>) -> (String, &'static str) {
        let db = self.db;
        // Special types like `<special-form of whatever 'blahblah' with 'florps'>`
        // render poorly with python syntax-highlighting but well as xml
        let settings = DisplaySettings::from_possibly_ambiguous_types(db, self.env, [*ty])
            .with_type_params_in_scope(db, self.type_params_in_scope.iter().cloned());
        let ty_string = ty
            .display_with(db, self.env, settings)
            .multiline()
            .to_string();
        let syntax = if ty_string.starts_with('<') {
            "xml"
        } else {
            "python"
        };
        (ty_string, syntax)
    }
}

fn create_qualifier_suffix(qualifiers: TypeQualifiers) -> String {
    let mut standard = qualifiers
        .iter()
        .filter(|q| !q.is_non_standard())
        .peekable();
    if standard.peek().is_none() {
        String::new()
    } else {
        let names: Vec<&str> = standard.map(TypeQualifiers::name).collect();
        format!(" ({})", names.join(", "))
    }
}

impl fmt::Display for DisplayHoverContent<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.content {
            HoverContent::Declaration(declaration) => {
                let name = declaration
                    .qualified_name
                    .as_deref()
                    .unwrap_or(&declaration.name);
                let kind = declaration.kind.as_str();
                match self.kind {
                    MarkupKind::PlainText => write!(f, "{kind} {name}"),
                    MarkupKind::Markdown => write!(f, "{kind} `{name}`"),
                }
            }
            HoverContent::DecoratedFunction {
                decorators,
                function,
            } => {
                let (signature, _) = self.ty_string_and_syntax(function);
                let mut written = decorators.join("\n");
                written.push('\n');
                written.push_str(&signature);
                self.kind.fenced_code_block(written, "python").fmt(f)
            }
            HoverContent::Signature(signature) => {
                self.kind.fenced_code_block(&signature, "python").fmt(f)
            }
            HoverContent::Parameter(parameter) => {
                self.kind.fenced_code_block(parameter, "python").fmt(f)
            }
            HoverContent::Type {
                ty,
                variance,
                qualifiers,
            } => {
                let variance = match variance {
                    Some(TypeVarVariance::Covariant) => " (covariant)",
                    Some(TypeVarVariance::Contravariant) => " (contravariant)",
                    Some(TypeVarVariance::Invariant) => " (invariant)",
                    Some(TypeVarVariance::Bivariant) => " (bivariant)",
                    None => "",
                };

                let qualifier_suffix = create_qualifier_suffix(*qualifiers);

                let (ty_string, syntax) = self.ty_string_and_syntax(ty);
                self.kind
                    .fenced_code_block(format!("{ty_string}{variance}{qualifier_suffix}"), syntax)
                    .fmt(f)
            }
            HoverContent::TypeAlias {
                declaration,
                qualifiers,
            } => {
                let qualifier_suffix = create_qualifier_suffix(*qualifiers);

                self.kind
                    .fenced_code_block(format!("{declaration}{qualifier_suffix}"), "python")
                    .fmt(f)
            }
            HoverContent::TypedDictKey { owner, key, ty } => {
                let (ty_string, syntax) = self.ty_string_and_syntax(ty);
                self.kind
                    .fenced_code_block(format!("(key of {owner}) {key}: {ty_string}"), syntax)
                    .fmt(f)
            }
            HoverContent::Docstring(docstring) => docstring.render(self.kind).fmt(f),
            HoverContent::DocstringFragment(fragment) => fragment.render(self.kind).fmt(f),
            HoverContent::FormatSpec(spec) => spec.fmt(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::{CursorTest, cursor_test};
    use crate::{MarkupKind, hover};
    use std::fmt::Write as _;

    use insta::assert_snapshot;
    use ruff_db::diagnostic::{
        Annotation, Diagnostic, DiagnosticId, DisplayDiagnosticConfig, LintName, Severity, Span,
    };
    use ruff_python_ast::PythonVersion;
    use ruff_text_size::{Ranged, TextRange};

    fn hover_test(source: &str) -> CursorTest {
        // Hover snapshots include markdown-rendered docstrings. Normalize markdown hard breaks
        // so snapshot literals remain stable even if an editor trims trailing whitespace.
        CursorTest::builder()
            .snapshot_filter("  \n", "<HB>\n")
            .source("main.py", source)
            .build()
    }

    #[test]
    fn hover_format_spec_previews_the_output() {
        let test = hover_test(
            r#"
        value = 1234.5678
        print(f"{value:>12<CURSOR>,.2f}")
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        format spec `>12,.2f`

        `1234.5678` formats as `    1,234.57`

        - `>` align — right
        - `12` width — the minimum number of characters
        - `,` grouping — every 3 digits
        - `.2` precision — digits after the point, significant digits, or the length a string is truncated to
        - `f` presentation type — fixed point

        ---------------------------------------------
        format spec `>12,.2f`

        `1234.5678` formats as `    1,234.57`

        - `>` align — right
        - `12` width — the minimum number of characters
        - `,` grouping — every 3 digits
        - `.2` precision — digits after the point, significant digits, or the length a string is truncated to
        - `f` presentation type — fixed point

        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:16
          |
        3 | print(f"{value:>12,.2f}")
          |                ^^^-^^^
          |                |  |
          |                |  Cursor offset
          |                source
        "#);
    }

    #[test]
    fn hover_format_spec_on_a_string() {
        let test = hover_test(
            r#"
        name = "spam"
        print(f"{name:*^1<CURSOR>0}")
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        format spec `*^10`

        `"spam"` formats as `***spam***`

        - `*` fill — the character the padding is made of
        - `^` align — centred
        - `10` width — the minimum number of characters

        ---------------------------------------------
        format spec `*^10`

        `"spam"` formats as `***spam***`

        - `*` fill — the character the padding is made of
        - `^` align — centred
        - `10` width — the minimum number of characters

        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:15
          |
        3 | print(f"{name:*^10}")
          |               ^^^-
          |               |  |
          |               |  Cursor offset
          |               source
        "#);
    }

    #[test]
    fn hover_format_spec_the_value_rejects() {
        let test = hover_test(
            r#"
        name = "spam"
        print(f"{name:<CURSOR>+}")
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        format spec `+`

        this spec is not one the value accepts

        - `+` sign — always signed

        ---------------------------------------------
        format spec `+`

        this spec is not one the value accepts

        - `+` sign — always signed

        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:15
          |
        3 | print(f"{name:+}")
          |               ^
          |               |
          |               source
          |               Cursor offset
        "#);
    }

    #[test]
    fn hover_strftime_spec_of_literal_text() {
        let test = hover_test(
            r#"
        import datetime
        print(f"{datetime.date.today():>1<CURSOR>0}")
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        format spec `>10`

        `datetime(2001, 2, 3, 4, 5, 6, 7)` formats as `>10`

        ---------------------------------------------
        format spec `>10`

        `datetime(2001, 2, 3, 4, 5, 6, 7)` formats as `>10`

        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:32
          |
        3 | print(f"{datetime.date.today():>10}")
          |                                ^^-
          |                                | |
          |                                | Cursor offset
          |                                source
        "#);
    }

    #[test]
    fn hover_format_spec_of_an_unknown_target() {
        let test = hover_test(
            r#"
        class Opaque:
            def __format__(self, spec: str, /) -> str: ...

        print(f"{Opaque():>1<CURSOR>0}")
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        format spec `>10`

        - `>` align — right
        - `10` width — the minimum number of characters

        ---------------------------------------------
        format spec `>10`

        - `>` align — right
        - `10` width — the minimum number of characters

        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:5:19
          |
        5 | print(f"{Opaque():>10}")
          |                   ^^-
          |                   | |
          |                   | Cursor offset
          |                   source
        "#);
    }

    #[test]
    fn hover_strftime_spec_previews_the_output() {
        let test = hover_test(
            r#"
        import datetime
        print(f"{datetime.date.today():%Y-<CURSOR>%m-%d}")
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        format spec `%Y-%m-%d`

        `datetime(2001, 2, 3, 4, 5, 6, 7)` formats as `2001-02-03`

        - `%Y` year with the century
        - `%m` month as a number, zero padded
        - `%d` day of the month, zero padded

        ---------------------------------------------
        format spec `%Y-%m-%d`

        `datetime(2001, 2, 3, 4, 5, 6, 7)` formats as `2001-02-03`

        - `%Y` year with the century
        - `%m` month as a number, zero padded
        - `%d` day of the month, zero padded

        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:32
          |
        3 | print(f"{datetime.date.today():%Y-%m-%d}")
          |                                ^^^-^^^^
          |                                |  |
          |                                |  Cursor offset
          |                                source
        "#);
    }

    #[test]
    fn hover_inside_a_nested_field_is_the_expression() {
        let test = hover_test(
            r#"
        value = 1234.5678
        width = 12
        print(f"{value:>{wid<CURSOR>th}.2f}")
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        variable main.width
        ---------------------------------------------
        Literal[12]
        ---------------------------------------------
        variable `main.width`
        ---
        ```python
        Literal[12]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:18
          |
        4 | print(f"{value:>{width}.2f}")
          |                  ^^^-^
          |                  |  |
          |                  |  Cursor offset
          |                  source
        "#);
    }

    #[test]
    fn hover_basic() {
        let test = hover_test(
            r#"
        a = 10
        """This is the docs for this value

        Wow these are good docs!
        """

        a<CURSOR>
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.a
        ---------------------------------------------
        Literal[10]
        ---------------------------------------------
        This is the docs for this value

        Wow these are good docs!

        ---------------------------------------------
        variable `main.a`
        ---
        ```python
        Literal[10]
        ```
        ---
        This is the docs for this value<HB>
        <HB>
        Wow these are good docs!
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:8:1
          |
        8 | a
          | ^- Cursor offset
          | |
          | source
        ");
    }

    #[test]
    fn hover_dynamic_class_with_markdown_fence() {
        let test = hover_test(
            r#"
        Injected = type("name\n```\n![image](https://example.invalid/)\n```", (), {})
        value = Injected()
        value<CURSOR>
        "#,
        );

        let rendered = hover(
            &test.db,
            test.program_file(test.cursor.file),
            test.cursor.offset,
        )
        .map(|hover| {
            (
                hover.display(&test.db, MarkupKind::PlainText).to_string(),
                hover.display(&test.db, MarkupKind::Markdown).to_string(),
            )
        });

        let code = "name\n```\n![image](https://example.invalid/)\n```";
        assert_eq!(
            rendered,
            Some((
                format!(
                    "variable main.value\n---------------------------------------------\n{code}"
                ),
                format!("variable `main.value`\n---\n````python\n{code}\n````")
            ))
        );
    }

    #[test]
    fn hover_function() {
        let test = hover_test(
            r#"
        def my_func(a, b):
            '''This is such a great func!!

            Args:
                a: first for a reason
                b: coming for `a`'s title
            '''
            return 0

        my_fu<CURSOR>nc(1, 2)
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.my_func
        ---------------------------------------------
        def my_func(
            a,
            b
        ) -> Literal[0]
        ---------------------------------------------
        This is such a great func!!

        Args:
            a: first for a reason
            b: coming for `a`'s title

        ---------------------------------------------
        function `main.my_func`
        ---
        ```python
        def my_func(
            a,
            b
        ) -> Literal[0]
        ```
        ---
        This is such a great func!!

        ## Arguments
        **a**<HB>
        first for a reason

        **b**<HB>
        coming for `a`'s title
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:11:1
           |
        11 | my_func(1, 2)
           | ^^^^^-^
           | |    |
           | |    Cursor offset
           | source
        ");
    }

    #[test]
    fn hover_function_def() {
        let test = hover_test(
            r#"
        def my_fu<CURSOR>nc(a, b):
            '''This is such a great func!!

            Args:
                a: first for a reason
                b: coming for `a`'s title
            '''
            return 0
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.my_func
        ---------------------------------------------
        def my_func(
            a,
            b
        ) -> Literal[0]
        ---------------------------------------------
        This is such a great func!!

        Args:
            a: first for a reason
            b: coming for `a`'s title

        ---------------------------------------------
        function `main.my_func`
        ---
        ```python
        def my_func(
            a,
            b
        ) -> Literal[0]
        ```
        ---
        This is such a great func!!

        ## Arguments
        **a**<HB>
        first for a reason

        **b**<HB>
        coming for `a`'s title
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:5
          |
        2 | def my_func(a, b):
          |     ^^^^^-^
          |     |    |
          |     |    Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_function_rest_docstring() {
        let test = hover_test(
            r#"
        def documented(value):
            '''Return a value.

            :param str value: The input value.
            :rtype: str
            '''
            return value

        docu<CURSOR>mented("x")
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        function main.documented
        ---------------------------------------------
        def documented(value) -> value
        ---------------------------------------------
        Return a value.

        :param str value: The input value.
        :rtype: str

        ---------------------------------------------
        function `main.documented`
        ---
        ```python
        def documented(value) -> value
        ```
        ---
        Return a value.

        ## Parameters
        **value**: `str`<HB>
        The input value.

        ## Returns
        `str`
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:10:1
           |
        10 | documented("x")
           | ^^^^-^^^^^
           | |   |
           | |   Cursor offset
           | source
        "#);
    }

    #[test]
    fn hover_class() {
        let test = hover_test(
            r#"
        class MyClass:
            '''
                This is such a great class!!

                    Don't you know?

                Everyone loves my class!!

            '''
            def __init__(self, val):
                """initializes MyClass (perfectly)"""
                self.val = val

            def my_method(self, a, b):
                '''This is such a great func!!

                Args:
                    a: first for a reason
                    b: coming for `a`'s title
                '''
                return 0

        MyCla<CURSOR>ss
        "#,
        );

        assert_snapshot!(test.hover(), @"
        class main.MyClass
        ---------------------------------------------
        <class 'MyClass'>
        ---------------------------------------------
        This is such a great class!!

            Don't you know?

        Everyone loves my class!!

        ---------------------------------------------
        class `main.MyClass`
        ---
        ```xml
        <class 'MyClass'>
        ```
        ---
        This is such a great class!!<HB>
        <HB>
        &nbsp;&nbsp;&nbsp;&nbsp;Don't you know?<HB>
        <HB>
        Everyone loves my class!!
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:24:1
           |
        24 | MyClass
           | ^^^^^-^
           | |    |
           | |    Cursor offset
           | source
        ");
    }

    #[test]
    fn hover_class_def() {
        let test = hover_test(
            r#"
        class MyCla<CURSOR>ss:
            '''
                This is such a great class!!

                    Don't you know?

                Everyone loves my class!!

            '''
            def __init__(self, val):
                """initializes MyClass (perfectly)"""
                self.val = val

            def my_method(self, a, b):
                '''This is such a great func!!

                Args:
                    a: first for a reason
                    b: coming for `a`'s title
                '''
                return 0
        "#,
        );

        assert_snapshot!(test.hover(), @"
        class main.MyClass
        ---------------------------------------------
        <class 'MyClass'>
        ---------------------------------------------
        This is such a great class!!

            Don't you know?

        Everyone loves my class!!

        ---------------------------------------------
        class `main.MyClass`
        ---
        ```xml
        <class 'MyClass'>
        ```
        ---
        This is such a great class!!<HB>
        <HB>
        &nbsp;&nbsp;&nbsp;&nbsp;Don't you know?<HB>
        <HB>
        Everyone loves my class!!
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:7
          |
        2 | class MyClass:
          |       ^^^^^-^
          |       |    |
          |       |    Cursor offset
          |       source
        ");
    }

    #[test]
    fn hover_class_init() {
        let test = hover_test(
            r#"
        class MyClass:
            '''
                This is such a great class!!

                    Don't you know?

                Everyone loves my class!!

            '''
            def __init__(self, val):
                """initializes MyClass (perfectly)"""
                self.val = val

            def my_method(self, a, b):
                '''This is such a great func!!

                Args:
                    a: first for a reason
                    b: coming for `a`'s title
                '''
                return 0

        x = MyCla<CURSOR>ss(0)
        "#,
        );

        assert_snapshot!(test.hover(), @"
        class main.MyClass
        ---------------------------------------------
        class MyClass(val)
        ---------------------------------------------
        initializes MyClass (perfectly)

        ---------------------------------------------
        class `main.MyClass`
        ---
        ```python
        class MyClass(val)
        ```
        ---
        initializes MyClass (perfectly)
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:24:5
           |
        24 | x = MyClass(0)
           |     ^^^^^-^
           |     |    |
           |     |    Cursor offset
           |     source
        ");
    }

    #[test]
    fn hover_class_init_attr() {
        let test = CursorTest::builder()
            .source(
                "mymod.py",
                r#"
        class MyClass:
            '''
                This is such a great class!!

                    Don't you know?

                Everyone loves my class!!

            '''
            def __init__(self, val):
                """initializes MyClass (perfectly)"""
                self.val = val
        "#,
            )
            .source(
                "main.py",
                r#"
        import mymod

        x = mymod.MyCla<CURSOR>ss(0)
        "#,
            )
            .build();

        assert_snapshot!(test.hover(), @"
        class mymod.MyClass
        ---------------------------------------------
        class MyClass(val)
        ---------------------------------------------
        initializes MyClass (perfectly)

        ---------------------------------------------
        class `mymod.MyClass`
        ---
        ```python
        class MyClass(val)
        ```
        ---
        initializes MyClass (perfectly)
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:11
          |
        4 | x = mymod.MyClass(0)
          |           ^^^^^-^
          |           |    |
          |           |    Cursor offset
          |           source
        ");
    }

    #[test]
    fn hover_class_init_no_init_docs() {
        let test = hover_test(
            r#"
        class MyClass:
            '''
                This is such a great class!!

                    Don't you know?

                Everyone loves my class!!

            '''
            def __init__(self, val):
                self.val = val

            def my_method(self, a, b):
                '''This is such a great func!!

                Args:
                    a: first for a reason
                    b: coming for `a`'s title
                '''
                return 0

        x = MyCla<CURSOR>ss(0)
        "#,
        );

        assert_snapshot!(test.hover(), @"
        class main.MyClass
        ---------------------------------------------
        class MyClass(val)
        ---------------------------------------------
        This is such a great class!!

            Don't you know?

        Everyone loves my class!!

        ---------------------------------------------
        class `main.MyClass`
        ---
        ```python
        class MyClass(val)
        ```
        ---
        This is such a great class!!<HB>
        <HB>
        &nbsp;&nbsp;&nbsp;&nbsp;Don't you know?<HB>
        <HB>
        Everyone loves my class!!
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:23:5
           |
        23 | x = MyClass(0)
           |     ^^^^^-^
           |     |    |
           |     |    Cursor offset
           |     source
        ");
    }

    #[test]
    fn hover_class_typed_init() {
        let test = hover_test(
            r#"
        class MyClass:
            def __init__(self, a: int, b: str):
                self.a = a
                self.b = b

        x = MyCla<CURSOR>ss(0, "hello")
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.MyClass
        ---------------------------------------------
        class MyClass(
            a: int,
            b: str
        )
        ---------------------------------------------
        class `main.MyClass`
        ---
        ```python
        class MyClass(
            a: int,
            b: str
        )
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:7:5
          |
        7 | x = MyClass(0, "hello")
          |     ^^^^^-^
          |     |    |
          |     |    Cursor offset
          |     source
        "#);
    }

    #[test]
    fn hover_dataclass_class_init() {
        let test = hover_test(
            r#"
        from dataclasses import dataclass

        @dataclass
        class MyClass:
            '''
                MyClass docs
            '''
            a: int
            b: str

        x = MyCla<CURSOR>ss(0, "")
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.MyClass
        ---------------------------------------------
        class MyClass(
            a: int,
            b: str
        )
        ---------------------------------------------
        MyClass docs

        ---------------------------------------------
        class `main.MyClass`
        ---
        ```python
        class MyClass(
            a: int,
            b: str
        )
        ```
        ---
        MyClass docs
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:12:5
           |
        12 | x = MyClass(0, "")
           |     ^^^^^-^
           |     |    |
           |     |    Cursor offset
           |     source
        "#);
    }

    #[test]
    fn hover_class_no_init() {
        let test = hover_test(
            r#"
        class MyClass:
            pass

        x = MyCla<CURSOR>ss()
        "#,
        );

        assert_snapshot!(test.hover(), @"
        class main.MyClass
        ---------------------------------------------
        class MyClass()
        ---------------------------------------------
        class `main.MyClass`
        ---
        ```python
        class MyClass()
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:5:5
          |
        5 | x = MyClass()
          |     ^^^^^-^
          |     |    |
          |     |    Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_class_with_new() {
        let test = hover_test(
            r#"
        class MyClass:
            def __new__(cls, a: int, b: str) -> "MyClass":
                instance = super().__new__(cls)
                return instance

        x = MyCla<CURSOR>ss(0, "hello")
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.MyClass
        ---------------------------------------------
        class MyClass(
            a: int,
            b: str
        )
        ---------------------------------------------
        class `main.MyClass`
        ---
        ```python
        class MyClass(
            a: int,
            b: str
        )
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:7:5
          |
        7 | x = MyClass(0, "hello")
          |     ^^^^^-^
          |     |    |
          |     |    Cursor offset
          |     source
        "#);
    }

    #[test]
    fn hover_class_init_overload_no_match() {
        let test = hover_test(
            r#"
        from typing import overload

        class Shape:
            """Shape docs"""

            @overload
            def __init__(self, val: str) -> None: ...

            @overload
            def __init__(self, val: int) -> None: ...

            def __init__(self, val: int | str) -> None:
                self.name = val

        x = Sha<CURSOR>pe()
        "#,
        );

        assert_snapshot!(test.hover(), @"
        class main.Shape
        ---------------------------------------------
        class Shape(val: str)
        class Shape(val: int)
        ---------------------------------------------
        Shape docs

        ---------------------------------------------
        class `main.Shape`
        ---
        ```python
        class Shape(val: str)
        class Shape(val: int)
        ```
        ---
        Shape docs
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:16:5
           |
        16 | x = Shape()
           |     ^^^-^
           |     |  |
           |     |  Cursor offset
           |     source
        ");
    }

    #[test]
    fn hover_class_init_overload_match() {
        let test = hover_test(
            r#"
        from typing import overload

        class Shape:
            """Shape docs"""

            @overload
            def __init__(self, val: str) -> None: ...

            @overload
            def __init__(self, val: int) -> None: ...

            def __init__(self, val: int | str) -> None:
                self.name = val

        x = Sha<CURSOR>pe("hello")
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.Shape
        ---------------------------------------------
        class Shape(val: str)
        ---------------------------------------------
        Shape docs

        ---------------------------------------------
        class `main.Shape`
        ---
        ```python
        class Shape(val: str)
        ```
        ---
        Shape docs
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:16:5
           |
        16 | x = Shape("hello")
           |     ^^^-^
           |     |  |
           |     |  Cursor offset
           |     source
        "#);
    }

    #[test]
    fn hover_class_init_and_new_invalid() {
        let test = hover_test(
            r#"
        class S:
            def __init__(self, a: int):
                """init docs"""
                pass

            def __new__(cls, a: int, b: str) -> "S":
                """new docs"""
                instance = super().__new__(cls)
                return instance

        x = <CURSOR>S(1)
        "#,
        );

        assert_snapshot!(test.hover(), @"
        class main.S
        ---------------------------------------------
        class S(
            a: int,
            b: str
        )
        class S(a: int)
        ---------------------------------------------
        init docs

        ---------------------------------------------
        class `main.S`
        ---
        ```python
        class S(
            a: int,
            b: str
        )
        class S(a: int)
        ```
        ---
        init docs
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:12:5
           |
        12 | x = S(1)
           |     ^
           |     |
           |     source
           |     Cursor offset
        ");
    }

    #[test]
    fn hover_class_init_and_new_valid() {
        let test = hover_test(
            r#"
        class S:
            def __init__(self, a: int):
                """init docs"""
                pass

            def __new__(cls, a: int) -> "S":
                """new docs"""
                instance = super().__new__(cls)
                return instance

        x = <CURSOR>S(1)
        "#,
        );

        assert_snapshot!(test.hover(), @"
        class main.S
        ---------------------------------------------
        class S(a: int)
        ---------------------------------------------
        new docs

        ---------------------------------------------
        class `main.S`
        ---
        ```python
        class S(a: int)
        ```
        ---
        new docs
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:12:5
           |
        12 | x = S(1)
           |     ^
           |     |
           |     source
           |     Cursor offset
        ");
    }

    #[test]
    fn hover_class_init_with_callable_param() {
        let test = hover_test(
            r#"
        from typing import Callable

        class Handler:
            def __init__(self, callback: Callable[[int, str], bool]):
                self.callback = callback

        x = Hand<CURSOR>ler(lambda i, s: True)
        "#,
        );

        assert_snapshot!(test.hover(), @"
        class main.Handler
        ---------------------------------------------
        class Handler(callback: (int, str, /) -> bool)
        ---------------------------------------------
        class `main.Handler`
        ---
        ```python
        class Handler(callback: (int, str, /) -> bool)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:8:5
          |
        8 | x = Handler(lambda i, s: True)
          |     ^^^^-^^
          |     |   |
          |     |   Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_enum_constructor() {
        let test = hover_test(
            r#"
        from enum import Enum

        class Color(Enum):
            RED = 1
            BLUE = 2

        x = Col<CURSOR>or(1)
        "#,
        );

        assert_snapshot!(test.hover(), @"
        class main.Color
        ---------------------------------------------
        class Color(value: object)
        ---------------------------------------------
        class `main.Color`
        ---
        ```python
        class Color(value: object)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:8:5
          |
        8 | x = Color(1)
          |     ^^^-^
          |     |  |
          |     |  Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_typeddict_constructor() {
        let test = hover_test(
            r#"
        from typing import TypedDict

        class Movie(TypedDict):
            title: str
            year: int

        x = Mov<CURSOR>ie(title="Alien", year=1979)
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.Movie
        ---------------------------------------------
        class Movie(
            *,
            title: str,
            year: int
        )
        ---------------------------------------------
        class `main.Movie`
        ---
        ```python
        class Movie(
            *,
            title: str,
            year: int
        )
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:8:5
          |
        8 | x = Movie(title="Alien", year=1979)
          |     ^^^-^
          |     |  |
          |     |  Cursor offset
          |     source
        "#);
    }

    #[test]
    fn hover_typeddict_constructor_positional_map() {
        let test = hover_test(
            r#"
        from typing import TypedDict

        class Movie(TypedDict):
            title: str
            year: int

        m: Movie = {"title": "Alien", "year": 1979}
        x = Mov<CURSOR>ie(m)
        "#,
        );

        assert_snapshot!(test.hover(), @"
        class main.Movie
        ---------------------------------------------
        class Movie(
            map: Movie,
            /,
            *,
            title: str = ...,
            year: int = ...
        )
        ---------------------------------------------
        class `main.Movie`
        ---
        ```python
        class Movie(
            map: Movie,
            /,
            *,
            title: str = ...,
            year: int = ...
        )
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:9:5
          |
        9 | x = Movie(m)
          |     ^^^-^
          |     |  |
          |     |  Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_typeddict_constructor_positional_map_dict_literal_in_constructor() {
        let test = hover_test(
            r#"
        from typing import TypedDict

        class Movie(TypedDict):
            title: str
            year: int

        x = Mov<CURSOR>ie({"title": "Alien", "year": 1979})
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.Movie
        ---------------------------------------------
        class Movie(
            map: Movie,
            /,
            *,
            title: str = ...,
            year: int = ...
        )
        ---------------------------------------------
        class `main.Movie`
        ---
        ```python
        class Movie(
            map: Movie,
            /,
            *,
            title: str = ...,
            year: int = ...
        )
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:8:5
          |
        8 | x = Movie({"title": "Alien", "year": 1979})
          |     ^^^-^
          |     |  |
          |     |  Cursor offset
          |     source
        "#);
    }

    #[test]
    fn hover_typeddict_constructor_not_required() {
        let test = hover_test(
            r#"
        from typing import TypedDict, NotRequired

        class Movie(TypedDict):
            title: str
            year: NotRequired[int]

        x = Mov<CURSOR>ie(title="Alien")
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.Movie
        ---------------------------------------------
        class Movie(
            *,
            title: str,
            year: int = ...
        )
        ---------------------------------------------
        class `main.Movie`
        ---
        ```python
        class Movie(
            *,
            title: str,
            year: int = ...
        )
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:8:5
          |
        8 | x = Movie(title="Alien")
          |     ^^^-^
          |     |  |
          |     |  Cursor offset
          |     source
        "#);
    }

    #[test]
    fn hover_typeddict_constructor_total_false() {
        let test = hover_test(
            r#"
        from typing import TypedDict

        class Movie(TypedDict, total=False):
            title: str
            year: int

        x = Mov<CURSOR>ie()
        "#,
        );

        assert_snapshot!(test.hover(), @"
        class main.Movie
        ---------------------------------------------
        class Movie(
            *,
            title: str = ...,
            year: int = ...
        )
        ---------------------------------------------
        class `main.Movie`
        ---
        ```python
        class Movie(
            *,
            title: str = ...,
            year: int = ...
        )
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:8:5
          |
        8 | x = Movie()
          |     ^^^-^
          |     |  |
          |     |  Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_class_method() {
        let test = hover_test(
            r#"
        class MyClass:
            '''
                This is such a great class!!

                    Don't you know?

                Everyone loves my class!!

            '''
            def __init__(self, val):
                """initializes MyClass (perfectly)"""
                self.val = val

            def my_method(self, a, b):
                '''This is such a great func!!

                Args:
                    a: first for a reason
                    b: coming for `a`'s title
                '''
                return 0

        x = MyClass(0)
        x.my_me<CURSOR>thod(2, 3)
        "#,
        );

        assert_snapshot!(test.hover(), @"
        method main.MyClass.my_method
        ---------------------------------------------
        bound method MyClass.my_method(
            a,
            b
        ) -> Literal[0]
        ---------------------------------------------
        This is such a great func!!

        Args:
            a: first for a reason
            b: coming for `a`'s title

        ---------------------------------------------
        method `main.MyClass.my_method`
        ---
        ```python
        bound method MyClass.my_method(
            a,
            b
        ) -> Literal[0]
        ```
        ---
        This is such a great func!!

        ## Arguments
        **a**<HB>
        first for a reason

        **b**<HB>
        coming for `a`'s title
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:25:3
           |
        25 | x.my_method(2, 3)
           |   ^^^^^-^^^
           |   |    |
           |   |    Cursor offset
           |   source
        ");
    }

    #[test]
    fn hover_overloaded_method_implementation_docstring() {
        let test = cursor_test(
            r#"
        from typing import overload

        class MyTestClass:
            @overload
            def foo(self, x: int) -> int: ...
            @overload
            def foo(self, x: str) -> str: ...
            def foo(self, x: int | str) -> int | str:
                """Sample docstring"""
                return x

        my_class = MyTestClass()
        my_class.f<CURSOR>oo(1)
        "#,
        );

        assert_snapshot!(test.hover(), @"
        method main.MyTestClass.foo
        ---------------------------------------------
        def foo(x: int) -> int
        ---------------------------------------------
        Sample docstring

        ---------------------------------------------
        method `main.MyTestClass.foo`
        ---
        ```python
        def foo(x: int) -> int
        ```
        ---
        Sample docstring
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:14:10
           |
        14 | my_class.foo(1)
           |          ^-^
           |          ||
           |          |Cursor offset
           |          source
        ");
    }

    #[test]
    fn hover_bound_method_overload_self_type() {
        let string = hover_test(
            r#"
        def f(string: str):
            string.removesuf<CURSOR>fix("suffix")
        "#,
        );

        assert_snapshot!(string.hover(), @r#"
        method builtins.str.removesuffix
        ---------------------------------------------
        bound method str.removesuffix(suffix: str, /) -> str
        ---------------------------------------------
        Return a str with the given suffix string removed if present.

        If the string ends with the suffix string and that suffix is not
        empty, return string[:-len(suffix)].  Otherwise, return a copy of
        the original string.

        ---------------------------------------------
        method `builtins.str.removesuffix`
        ---
        ```python
        bound method str.removesuffix(suffix: str, /) -> str
        ```
        ---
        Return a str with the given suffix string removed if present.<HB>
        <HB>
        If the string ends with the suffix string and that suffix is not<HB>
        empty, return string[:-len(suffix)].  Otherwise, return a copy of<HB>
        the original string.
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:12
          |
        3 |     string.removesuffix("suffix")
          |            ^^^^^^^^^-^^
          |            |        |
          |            |        Cursor offset
          |            source
        "#);

        let literal_string = hover_test(
            r#"
            from typing_extensions import LiteralString

            def f(string: LiteralString):
                string.removesuf<CURSOR>fix("suffix")
            "#,
        );

        assert_snapshot!(literal_string.hover(), @r#"
        method builtins.str.removesuffix
        ---------------------------------------------
        def removesuffix(suffix: LiteralString, /) -> LiteralString
        ---------------------------------------------
        Return a str with the given suffix string removed if present.

        If the string ends with the suffix string and that suffix is not
        empty, return string[:-len(suffix)].  Otherwise, return a copy of
        the original string.

        ---------------------------------------------
        method `builtins.str.removesuffix`
        ---
        ```python
        def removesuffix(suffix: LiteralString, /) -> LiteralString
        ```
        ---
        Return a str with the given suffix string removed if present.<HB>
        <HB>
        If the string ends with the suffix string and that suffix is not<HB>
        empty, return string[:-len(suffix)].  Otherwise, return a copy of<HB>
        the original string.
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:5:12
          |
        5 |     string.removesuffix("suffix")
          |            ^^^^^^^^^-^^
          |            |        |
          |            |        Cursor offset
          |            source
        "#);
    }

    /// When the resolved overload has no docstring and neither does the
    /// implementation, we fall back to showing a sibling overload's docstring.
    #[test]
    fn hover_overloaded_function_uses_sibling_overload_docstring_as_fallback() {
        let test = cursor_test(
            r#"
        from typing import overload

        @overload
        def test() -> str: ...

        @overload
        def test(arg: str) -> str:
            """A second overload"""

        def test(arg: str | None = None) -> str:
            return "test"

        t<CURSOR>est()
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.test
        ---------------------------------------------
        def test() -> str
        ---------------------------------------------
        A second overload

        ---------------------------------------------
        function `main.test`
        ---
        ```python
        def test() -> str
        ```
        ---
        A second overload
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:14:1
           |
        14 | test()
           | ^-^^
           | ||
           | |Cursor offset
           | source
        ");
    }

    #[test]
    fn hover_non_callable_decorated_function_shows_docstring() {
        let test = cursor_test(
            r#"
        def decorator(f):
            return 42

        @decorator
        def foo():
            """Foo documentation"""
            pass

        f<CURSOR>oo()
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.foo
        ---------------------------------------------
        Literal[42]
        ---------------------------------------------
        @decorator
        def foo()
        ---------------------------------------------
        Foo documentation

        ---------------------------------------------
        function `main.foo`
        ---
        ```python
        Literal[42]
        ```
        ---
        ```python
        @decorator
        def foo()
        ```
        ---
        Foo documentation
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:10:1
           |
        10 | foo()
           | ^-^
           | ||
           | |Cursor offset
           | source
        ");
    }

    #[test]
    fn hover_overloaded_function_with_conditional_definitions() {
        let test = cursor_test(
            r#"
        from typing import overload, Any
        def foo() -> bool: ...

        @overload
        def test() -> None: ...

        if foo():
            @overload
            def test(a: str) -> str: ...
        else:
            @overload
            def test(a: int) -> int: ...

        def test(a: Any) -> Any:
            """Implementation docstring"""
            return a

        t<CURSOR>est()
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.test
        ---------------------------------------------
        def test()
        ---------------------------------------------
        Implementation docstring

        ---------------------------------------------
        function `main.test`
        ---
        ```python
        def test()
        ```
        ---
        Implementation docstring
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:19:1
           |
        19 | test()
           | ^-^^
           | ||
           | |Cursor offset
           | source
        ");
    }

    #[test]
    fn hover_overloaded_function_with_version_conditional_implementation() {
        let test = CursorTest::builder()
            .python_version(PythonVersion::PY310)
            .source(
                "main.py",
                r#"
        from typing import overload, Any
        import sys

        @overload
        def test() -> None: ...

        if sys.version_info >= (3, 10):
            @overload
            def test(a: str) -> str: ...

            def test(a: Any) -> Any:
                """Version 3.10+ implementation"""
                return a
        else:
            @overload
            def test(a: int) -> int: ...

            def test(a: Any) -> Any:
                """Fallback implementation"""
                return a

        t<CURSOR>est()
        "#,
            )
            .build();

        assert_snapshot!(test.hover(), @"
        function main.test
        ---------------------------------------------
        def test()
        ---------------------------------------------
        Version 3.10+ implementation

        ---------------------------------------------
        function `main.test`
        ---
        ```python
        def test()
        ```
        ---
        Version 3.10+ implementation
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:23:1
           |
        23 | test()
           | ^-^^
           | ||
           | |Cursor offset
           | source
        ");
    }

    /// The implementation docstring fallback uses type-aware overload matching
    /// to avoid picking up an unrelated conditional reassignment of the same name.
    #[test]
    fn hover_overloaded_function_conditionally_reassigned() {
        let test = cursor_test(
            r#"
        from typing import overload

        @overload
        def test(x: int) -> int: ...
        @overload
        def test(x: str) -> str: ...
        def test(x):
            return x

        def flag() -> bool: ...
        if flag():
            def test():
                """Unrelated docstring"""
                pass

        t<CURSOR>est(1)
        "#,
        );

        // The type is a union because `test` is conditionally reassigned.
        // The "Unrelated docstring" comes from the conditional reassignment's
        // definition, which is the first definition with a docstring found
        // by the fallback path.
        assert_snapshot!(test.hover(), @"
        function main.test
        ---------------------------------------------
        (Overload[(x: int) -> int, (x: str) -> str]) | (def test())
        ---------------------------------------------
        Unrelated docstring

        ---------------------------------------------
        function `main.test`
        ---
        ```python
        (Overload[(x: int) -> int, (x: str) -> str]) | (def test())
        ```
        ---
        Unrelated docstring
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:17:1
           |
        17 | test(1)
           | ^-^^
           | ||
           | |Cursor offset
           | source
        ");
    }

    /// Like [`hover_overloaded_function_conditionally_reassigned`], but the
    /// resolved overload itself carries a docstring. The signature path
    /// attaches that docstring directly, so the unrelated reassignment is
    /// never consulted.
    #[test]
    fn hover_overloaded_function_conditionally_reassigned_overload_has_docstring() {
        let test = cursor_test(
            r#"
        from typing import overload

        @overload
        def test(x: int) -> int:
            """The int overload"""
        @overload
        def test(x: str) -> str: ...
        def test(x):
            return x

        def flag() -> bool: ...
        if flag():
            def test():
                """Unrelated docstring"""
                pass

        t<CURSOR>est(1)
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.test
        ---------------------------------------------
        (Overload[(x: int) -> int, (x: str) -> str]) | (def test())
        ---------------------------------------------
        The int overload

        ---------------------------------------------
        function `main.test`
        ---
        ```python
        (Overload[(x: int) -> int, (x: str) -> str]) | (def test())
        ```
        ---
        The int overload
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:18:1
           |
        18 | test(1)
           | ^-^^
           | ||
           | |Cursor offset
           | source
        ");
    }

    /// Like [`hover_overloaded_function_conditionally_reassigned`], but the
    /// implementation carries a docstring. The type-aware filter in
    /// `implementation_docstring` keeps the real implementation (whose
    /// overload chain contains the resolved overload) and drops the
    /// unrelated reassignment.
    #[test]
    fn hover_overloaded_function_conditionally_reassigned_impl_has_docstring() {
        let test = cursor_test(
            r#"
        from typing import overload

        @overload
        def test(x: int) -> int: ...
        @overload
        def test(x: str) -> str: ...
        def test(x):
            """The real implementation"""
            return x

        def flag() -> bool: ...
        if flag():
            def test():
                """Unrelated docstring"""
                pass

        t<CURSOR>est(1)
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.test
        ---------------------------------------------
        (Overload[(x: int) -> int, (x: str) -> str]) | (def test())
        ---------------------------------------------
        The real implementation

        ---------------------------------------------
        function `main.test`
        ---
        ```python
        (Overload[(x: int) -> int, (x: str) -> str]) | (def test())
        ```
        ---
        The real implementation
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:18:1
           |
        18 | test(1)
           | ^-^^
           | ||
           | |Cursor offset
           | source
        ");
    }

    #[test]
    fn hover_member() {
        let test = hover_test(
            r#"
        class Foo:
            a: int = 10

            def __init__(a: int, b: str):
                self.a = a
                """This is the docs for this value

                Wow these are good docs!
                """
                self.b: str = b

        foo = Foo()
        foo.<CURSOR>a
        "#,
        );

        assert_snapshot!(test.hover(), @"
        attribute main.Foo.a
        ---------------------------------------------
        int
        ---------------------------------------------
        attribute `main.Foo.a`
        ---
        ```python
        int
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:14:5
           |
        14 | foo.a
           |     ^
           |     |
           |     source
           |     Cursor offset
        ");
    }

    #[test]
    fn hover_function_typed_variable() {
        let test = hover_test(
            r#"
            def foo(a, b): ...

            foo<CURSOR>
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.foo
        ---------------------------------------------
        def foo(
            a,
            b
        )
        ---------------------------------------------
        function `main.foo`
        ---
        ```python
        def foo(
            a,
            b
        )
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:1
          |
        4 | foo
          | ^^^- Cursor offset
          | |
          | source
        ");
    }

    #[test]
    fn hover_binary_expression() {
        let test = hover_test(
            r#"
            def foo(a: int, b: int, c: int):
                a + b ==<CURSOR> c
        "#,
        );

        assert_snapshot!(test.hover(), @"
        bool
        ---------------------------------------------
        ```python
        bool
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:5
          |
        3 |     a + b == c
          |     ^^^^^^^^-^
          |     |       |
          |     |       Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_keyword_parameter() {
        let test = hover_test(
            r#"
            def test(ab: int):
                """my cool test

                Args:
                    ab: a nice little integer
                """
                return 0

            test(a<CURSOR>b= 123)
            "#,
        );

        assert_snapshot!(test.hover(), @"
        (parameter) ab: int
        ---------------------------------------------
        a nice little integer

        ---------------------------------------------
        ```python
        (parameter) ab: int
        ```
        ---
        a nice little integer
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:10:6
           |
        10 | test(ab= 123)
           |      ^-
           |      ||
           |      |Cursor offset
           |      source
        ");
    }

    #[test]
    fn hover_keyword_parameter_preserves_rest_field_like_continuation() {
        let test = hover_test(
            r#"
            def test(value: int):
                """my cool test

                :param value:
                    :param fake: - Parent option.
                        - Child option.
                """
                return 0

            test(value<CURSOR>=123)
            "#,
        );

        assert_snapshot!(test.hover(), @"
        (parameter) value: int
        ---------------------------------------------
        :param fake: - Parent option.
            - Child option.

        ---------------------------------------------
        ```python
        (parameter) value: int
        ```
        ---
        :param fake: - Parent option.<HB>
            - Child option.
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:11:6
           |
        11 | test(value=123)
           |      ^^^^^- Cursor offset
           |      |
           |      source
        ");
    }

    #[test]
    fn hover_keyword_parameter_without_documentation() {
        let test = hover_test(
            r#"
            def test(ab: int):
                return 0

            test(a<CURSOR>b=123)
            "#,
        );

        assert_snapshot!(test.hover(), @"
        (parameter) ab: int
        ---------------------------------------------
        ```python
        (parameter) ab: int
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:5:6
          |
        5 | test(ab=123)
          |      ^-
          |      ||
          |      |Cursor offset
          |      source
        ");
    }

    #[test]
    fn hover_keyword_only_parameter() {
        let test = hover_test(
            r#"
            def test(*, ab: int):
                """my cool test

                Args:
                    ab: a keyword-only integer
                """
                return 0

            test(a<CURSOR>b=123)
            "#,
        );

        assert_snapshot!(test.hover(), @"
        (parameter) ab: int
        ---------------------------------------------
        a keyword-only integer

        ---------------------------------------------
        ```python
        (parameter) ab: int
        ```
        ---
        a keyword-only integer
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:10:6
           |
        10 | test(ab=123)
           |      ^-
           |      ||
           |      |Cursor offset
           |      source
        ");
    }

    #[test]
    fn hover_keyword_variadic_parameter() {
        let test = hover_test(
            r#"
            def test(**settings: int):
                """my cool test

                Args:
                    settings: extra integer settings
                """
                return 0

            test(de<CURSOR>bug=123)
            "#,
        );

        assert_snapshot!(test.hover(), @"
        (variadic keyword parameter) **settings: int
        ---------------------------------------------
        extra integer settings

        ---------------------------------------------
        ```python
        (variadic keyword parameter) **settings: int
        ```
        ---
        extra integer settings
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:10:6
           |
        10 | test(debug=123)
           |      ^^-^^
           |      | |
           |      | Cursor offset
           |      source
        ");
    }

    #[test]
    fn hover_keyword_parameter_overload_implementation_parameter_docs() {
        let test = hover_test(
            r#"
            from typing import overload

            @overload
            def choose(*, fallback: int) -> int: ...

            @overload
            def choose(*, fallback: str) -> str: ...

            def choose(*, fallback: int | str) -> int | str:
                """Choose a value.

                Args:
                    fallback: Returned when parsing fails.
                """
                return fallback

            choose(fall<CURSOR>back=0)
            "#,
        );

        // The parameter label comes from the resolved `int` overload, while
        // the parameter documentation comes from the implementation docstring.
        assert_snapshot!(test.hover(), @"
        (parameter) fallback: int
        ---------------------------------------------
        Returned when parsing fails.

        ---------------------------------------------
        ```python
        (parameter) fallback: int
        ```
        ---
        Returned when parsing fails.
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:18:8
           |
        18 | choose(fallback=0)
           |        ^^^^-^^^
           |        |   |
           |        |   Cursor offset
           |        source
        ");
    }

    #[test]
    fn hover_keyword_parameter_overload_docstring_blocks_implementation_parameter_docs() {
        let test = hover_test(
            r#"
            from typing import overload

            @overload
            def choose(*, fallback: int) -> int:
                """Integer overload."""

            @overload
            def choose(*, fallback: str) -> str: ...

            def choose(*, fallback: int | str) -> int | str:
                """Choose a value.

                Args:
                    fallback: Returned when parsing fails.
                """
                return fallback

            choose(fall<CURSOR>back=0)
            "#,
        );

        // The selected overload has a docstring, so we stop there and do not
        // merge in the  `fallback` documentation from the implementation
        // docstring. We may want to improve this behaviour in the future.
        assert_snapshot!(test.hover(), @"
        (parameter) fallback: int
        ---------------------------------------------
        ```python
        (parameter) fallback: int
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:19:8
           |
        19 | choose(fallback=0)
           |        ^^^^-^^^
           |        |   |
           |        |   Cursor offset
           |        source
        ");
    }

    #[test]
    fn hover_keyword_argument_value_keeps_value_type() {
        let test = hover_test(
            r#"
            value = 123

            def test(ab: int):
                """my cool test

                Args:
                    ab: a nice little integer
                """
                return 0

            test(ab=va<CURSOR>lue)
            "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.value
        ---------------------------------------------
        Literal[123]
        ---------------------------------------------
        variable `main.value`
        ---
        ```python
        Literal[123]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:12:9
           |
        12 | test(ab=value)
           |         ^^-^^
           |         | |
           |         | Cursor offset
           |         source
        ");
    }

    #[test]
    fn hover_keyword_parameter_def() {
        let test = hover_test(
            r#"
            def test(a<CURSOR>b: int):
                """my cool test

                Args:
                    ab: a nice little integer
                """
                return 0
            "#,
        );

        assert_snapshot!(test.hover(), @"
        parameter ab
        ---------------------------------------------
        int
        ---------------------------------------------
        parameter `ab`
        ---
        ```python
        int
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:10
          |
        2 | def test(ab: int):
          |          ^-
          |          ||
          |          |Cursor offset
          |          source
        ");
    }

    #[test]
    fn hover_union() {
        let test = hover_test(
            r#"

            def foo(a, b):
                """The foo function"""
                return 0

            def bar(a, b):
                """The bar function"""
                return 1

            if random.choice([True, False]):
                a = foo
            else:
                a = bar

            a<CURSOR>
            "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.a
        ---------------------------------------------
        (def foo(a, b) -> Literal[0]) | (def bar(a, b) -> Literal[1])
        ---------------------------------------------
        variable `main.a`
        ---
        ```python
        (def foo(a, b) -> Literal[0]) | (def bar(a, b) -> Literal[1])
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:16:1
           |
        16 | a
           | ^- Cursor offset
           | |
           | source
        ");
    }

    #[test]
    fn hover_string_annotation1() {
        let test = hover_test(
            r#"
        a: "MyCla<CURSOR>ss" = 1

        class MyClass:
            """some docs"""
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.MyClass
        ---------------------------------------------
        MyClass
        ---------------------------------------------
        some docs

        ---------------------------------------------
        class `main.MyClass`
        ---
        ```python
        MyClass
        ```
        ---
        some docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:5
          |
        2 | a: "MyClass" = 1
          |     ^^^^^-^
          |     |    |
          |     |    Cursor offset
          |     source
        "#);
    }

    #[test]
    fn hover_string_annotation2() {
        let test = hover_test(
            r#"
        a: "None | MyCl<CURSOR>ass" = 1

        class MyClass:
            """some docs"""
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.MyClass
        ---------------------------------------------
        MyClass
        ---------------------------------------------
        some docs

        ---------------------------------------------
        class `main.MyClass`
        ---
        ```python
        MyClass
        ```
        ---
        some docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:12
          |
        2 | a: "None | MyClass" = 1
          |            ^^^^-^^
          |            |   |
          |            |   Cursor offset
          |            source
        "#);
    }

    // Hovering over a union operator in a string annotation shows the Union documentation.
    #[test]
    fn hover_string_annotation3() {
        let test = hover_test(
            r#"
        a: "None |<CURSOR> MyClass" = 1

        class MyClass:
            """some docs"""
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        <special-form 'typing.Union'>
        ---------------------------------------------
        Represent a union type

        E.g. for int | str

        ---------------------------------------------
        ```xml
        <special-form 'typing.Union'>
        ```
        ---
        Represent a union type<HB>
        <HB>
        E.g. for int | str
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:10
          |
        2 | a: "None | MyClass" = 1
          |          ^- Cursor offset
          |          |
          |          source
        "#);
    }

    #[test]
    fn hover_string_annotation4() {
        let test = hover_test(
            r#"
        a: "None | MyClass<CURSOR>" = 1

        class MyClass:
            """some docs"""
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.MyClass
        ---------------------------------------------
        MyClass
        ---------------------------------------------
        some docs

        ---------------------------------------------
        class `main.MyClass`
        ---
        ```python
        MyClass
        ```
        ---
        some docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:12
          |
        2 | a: "None | MyClass" = 1
          |            ^^^^^^^- Cursor offset
          |            |
          |            source
        "#);
    }

    #[test]
    fn hover_string_annotation5() {
        let test = hover_test(
            r#"
        a: "None | MyClass"<CURSOR> = 1

        class MyClass:
            """some docs"""
        "#,
        );

        assert_snapshot!(test.hover(), @"Hover provided no content");
    }

    #[test]
    fn hover_string_annotation_dangling1() {
        let test = hover_test(
            r#"
        a: "MyCl<CURSOR>ass |" = 1

        class MyClass:
            """some docs"""
        "#,
        );

        assert_snapshot!(test.hover(), @"Hover provided no content");
    }

    #[test]
    fn hover_string_annotation_dangling2() {
        let test = hover_test(
            r#"
        a: "MyCl<CURSOR>ass | No" = 1

        class MyClass:
            """some docs"""
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.MyClass
        ---------------------------------------------
        MyClass
        ---------------------------------------------
        some docs

        ---------------------------------------------
        class `main.MyClass`
        ---
        ```python
        MyClass
        ```
        ---
        some docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:5
          |
        2 | a: "MyClass | No" = 1
          |     ^^^^-^^
          |     |   |
          |     |   Cursor offset
          |     source
        "#);
    }

    #[test]
    fn hover_string_annotation_dangling3() {
        let test = hover_test(
            r#"
        a: "MyClass | N<CURSOR>o" = 1

        class MyClass:
            """some docs"""
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        Unknown
        ---------------------------------------------
        ```python
        Unknown
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:15
          |
        2 | a: "MyClass | No" = 1
          |               ^-
          |               ||
          |               |Cursor offset
          |               source
        "#);
    }

    #[test]
    fn hover_string_annotation_recursive() {
        let test = hover_test(
            r#"
        ab: "a<CURSOR>b"
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        variable main.ab
        ---------------------------------------------
        Unknown
        ---------------------------------------------
        variable `main.ab`
        ---
        ```python
        Unknown
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:6
          |
        2 | ab: "ab"
          |      ^-
          |      ||
          |      |Cursor offset
          |      source
        "#);
    }

    #[test]
    fn hover_string_annotation_unknown() {
        let test = hover_test(
            r#"
        x: "foo<CURSOR>bar"
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        Unknown
        ---------------------------------------------
        ```python
        Unknown
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:5
          |
        2 | x: "foobar"
          |     ^^^-^^
          |     |  |
          |     |  Cursor offset
          |     source
        "#);
    }

    #[test]
    fn goto_type_string_annotation_nested1() {
        let test = hover_test(
            r#"
        x: "list['My<CURSOR>Class | int'] | None"

        class MyClass:
            """some docs"""
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.MyClass
        ---------------------------------------------
        MyClass
        ---------------------------------------------
        some docs

        ---------------------------------------------
        class `main.MyClass`
        ---
        ```python
        MyClass
        ```
        ---
        some docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:11
          |
        2 | x: "list['MyClass | int'] | None"
          |           ^^-^^^^
          |           | |
          |           | Cursor offset
          |           source
        "#);
    }

    #[test]
    fn goto_type_string_annotation_nested2() {
        let test = hover_test(
            r#"
        x: "list['int | My<CURSOR>Class'] | None"

        class MyClass:
            """some docs"""
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.MyClass
        ---------------------------------------------
        MyClass
        ---------------------------------------------
        some docs

        ---------------------------------------------
        class `main.MyClass`
        ---
        ```python
        MyClass
        ```
        ---
        some docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:17
          |
        2 | x: "list['int | MyClass'] | None"
          |                 ^^-^^^^
          |                 | |
          |                 | Cursor offset
          |                 source
        "#);
    }

    #[test]
    fn goto_type_string_annotation_nested3() {
        let test = hover_test(
            r#"
        x: "list['int | None'] | My<CURSOR>Class"

        class MyClass:
            """some docs"""
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.MyClass
        ---------------------------------------------
        MyClass
        ---------------------------------------------
        some docs

        ---------------------------------------------
        class `main.MyClass`
        ---
        ```python
        MyClass
        ```
        ---
        some docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:26
          |
        2 | x: "list['int | None'] | MyClass"
          |                          ^^-^^^^
          |                          | |
          |                          | Cursor offset
          |                          source
        "#);
    }

    #[test]
    fn goto_type_string_annotation_nested4() {
        let test = hover_test(
            r#"
        x: "list['int' | 'My<CURSOR>Class'] | None"

        class MyClass:
            """some docs"""
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.MyClass
        ---------------------------------------------
        MyClass
        ---------------------------------------------
        some docs

        ---------------------------------------------
        class `main.MyClass`
        ---
        ```python
        MyClass
        ```
        ---
        some docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:19
          |
        2 | x: "list['int' | 'MyClass'] | None"
          |                   ^^-^^^^
          |                   | |
          |                   | Cursor offset
          |                   source
        "#);
    }

    #[test]
    fn goto_type_string_annotation_nested5() {
        let test = hover_test(
            r#"
        x: "list['My<CURSOR>Class' | 'str'] | None"

        class MyClass:
            """some docs"""
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.MyClass
        ---------------------------------------------
        MyClass
        ---------------------------------------------
        some docs

        ---------------------------------------------
        class `main.MyClass`
        ---
        ```python
        MyClass
        ```
        ---
        some docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:11
          |
        2 | x: "list['MyClass' | 'str'] | None"
          |           ^^-^^^^
          |           | |
          |           | Cursor offset
          |           source
        "#);
    }

    #[test]
    fn goto_type_string_annotation_too_nested1() {
        let test = hover_test(
            r#"
        x: """'list["My<CURSOR>Class" | "str"]' | None"""

        class MyClass:
            """some docs"""
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        Unknown
        ---------------------------------------------
        ```python
        Unknown
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:13
          |
        2 | x: """'list["MyClass" | "str"]' | None"""
          |             ^^^-^^^^^
          |             |  |
          |             |  Cursor offset
          |             source
        "#);
    }

    #[test]
    fn goto_type_string_annotation_too_nested2() {
        let test = hover_test(
            r#"
        x: """'list["int" | "str"]' | My<CURSOR>Class"""

        class MyClass:
            """some docs"""
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        class main.MyClass
        ---------------------------------------------
        MyClass
        ---------------------------------------------
        some docs

        ---------------------------------------------
        class `main.MyClass`
        ---
        ```python
        MyClass
        ```
        ---
        some docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:31
          |
        2 | x: """'list["int" | "str"]' | MyClass"""
          |                               ^^-^^^^
          |                               | |
          |                               | Cursor offset
          |                               source
        "#);
    }

    #[test]
    fn hover_overload_type_disambiguated1() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import ab

a<CURSOR>b(1)
",
            )
            .source(
                "mymodule.py",
                r#"
def ab(a):
    """the real implementation!"""
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
from typing import overload

@overload
def ab(a: int):
    """the int overload"""

@overload
def ab(a: str): ...
    """the str overload"""
"#,
            )
            .build();

        assert_snapshot!(test.hover(), @"
        function mymodule.ab
        ---------------------------------------------
        def ab(a: int)
        ---------------------------------------------
        the int overload

        ---------------------------------------------
        function `mymodule.ab`
        ---
        ```python
        def ab(a: int)
        ```
        ---
        the int overload
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:1
          |
        4 | ab(1)
          | ^-
          | ||
          | |Cursor offset
          | source
        ");
    }

    #[test]
    fn hover_reexported_stub_definition_implementation_docstring() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from a import bar

ba<CURSOR>r
",
            )
            .source("a/__init__.pyi", "def bar() -> None: ...\n")
            .source("a/__init__.py", "from .impl import bar as bar\n")
            .source(
                "a/impl.py",
                r#"
def bar() -> None:
    """Implementation docstring"""
"#,
            )
            .build();

        assert_snapshot!(test.hover(), @"
        function a.bar
        ---------------------------------------------
        def bar()
        ---------------------------------------------
        Implementation docstring

        ---------------------------------------------
        function `a.bar`
        ---
        ```python
        def bar()
        ```
        ---
        Implementation docstring
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:1
          |
        4 | bar
          | ^^-
          | | |
          | | Cursor offset
          | source
        ");
    }

    #[test]
    fn hover_overload_type_disambiguated2() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
from mymodule import ab

a<CURSOR>b("hello")
"#,
            )
            .source(
                "mymodule.py",
                r#"
def ab(a):
    """the real implementation!"""
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
from typing import overload

@overload
def ab(a: int):
    """the int overload"""

@overload
def ab(a: str):
    """the str overload"""
"#,
            )
            .build();

        assert_snapshot!(test.hover(), @r#"
        function mymodule.ab
        ---------------------------------------------
        def ab(a: str)
        ---------------------------------------------
        the str overload

        ---------------------------------------------
        function `mymodule.ab`
        ---
        ```python
        def ab(a: str)
        ```
        ---
        the str overload
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:1
          |
        4 | ab("hello")
          | ^-
          | ||
          | |Cursor offset
          | source
        "#);
    }

    #[test]
    fn hover_overload_arity_disambiguated1() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import ab

a<CURSOR>b(1, 2)
",
            )
            .source(
                "mymodule.py",
                r#"
def ab(a, b = None):
    """the real implementation!"""
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
from typing import overload

@overload
def ab(a: int, b: int):
    """the two arg overload"""

@overload
def ab(a: int):
    """the one arg overload"""
"#,
            )
            .build();

        assert_snapshot!(test.hover(), @"
        function mymodule.ab
        ---------------------------------------------
        def ab(
            a: int,
            b: int
        )
        ---------------------------------------------
        the two arg overload

        ---------------------------------------------
        function `mymodule.ab`
        ---
        ```python
        def ab(
            a: int,
            b: int
        )
        ```
        ---
        the two arg overload
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:1
          |
        4 | ab(1, 2)
          | ^-
          | ||
          | |Cursor offset
          | source
        ");
    }

    #[test]
    fn hover_overload_arity_disambiguated2() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import ab

a<CURSOR>b(1)
",
            )
            .source(
                "mymodule.py",
                r#"
def ab(a, b = None):
    """the real implementation!"""
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
from typing import overload

@overload
def ab(a: int, b: int):
    """the two arg overload"""

@overload
def ab(a: int):
    """the one arg overload"""
"#,
            )
            .build();

        assert_snapshot!(test.hover(), @"
        function mymodule.ab
        ---------------------------------------------
        def ab(a: int)
        ---------------------------------------------
        the one arg overload

        ---------------------------------------------
        function `mymodule.ab`
        ---
        ```python
        def ab(a: int)
        ```
        ---
        the one arg overload
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:1
          |
        4 | ab(1)
          | ^-
          | ||
          | |Cursor offset
          | source
        ");
    }

    #[test]
    fn hover_overload_keyword_disambiguated1() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import ab

a<CURSOR>b(1, b=2)
",
            )
            .source(
                "mymodule.py",
                r#"
def ab(a, *, b = None, c = None):
    """the real implementation!"""
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
from typing import overload

@overload
def ab(a: int):
    """keywordless overload"""

@overload
def ab(a: int, *, b: int):
    """b overload"""

@overload
def ab(a: int, *, c: int):
    """c overload"""
"#,
            )
            .build();

        assert_snapshot!(test.hover(), @"
        function mymodule.ab
        ---------------------------------------------
        def ab(
            a: int,
            *,
            b: int
        )
        ---------------------------------------------
        b overload

        ---------------------------------------------
        function `mymodule.ab`
        ---
        ```python
        def ab(
            a: int,
            *,
            b: int
        )
        ```
        ---
        b overload
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:1
          |
        4 | ab(1, b=2)
          | ^-
          | ||
          | |Cursor offset
          | source
        ");
    }

    #[test]
    fn hover_overload_keyword_disambiguated2() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import ab

a<CURSOR>b(1, c=2)
",
            )
            .source(
                "mymodule.py",
                r#"
def ab(a, *, b = None, c = None):
    """the real implementation!"""
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
from typing import overload

@overload
def ab(a: int):
    """keywordless overload"""

@overload
def ab(a: int, *, b: int):
    """b overload"""

@overload
def ab(a: int, *, c: int):
    """c overload"""
"#,
            )
            .build();

        assert_snapshot!(test.hover(), @"
        function mymodule.ab
        ---------------------------------------------
        def ab(
            a: int,
            *,
            c: int
        )
        ---------------------------------------------
        c overload

        ---------------------------------------------
        function `mymodule.ab`
        ---
        ```python
        def ab(
            a: int,
            *,
            c: int
        )
        ```
        ---
        c overload
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:1
          |
        4 | ab(1, c=2)
          | ^-
          | ||
          | |Cursor offset
          | source
        ");
    }

    #[test]
    fn hover_overload_ambiguous() {
        let test = hover_test(
            r#"
            from typing import overload

            @overload
            def foo(a: int, b):
                """The first overload"""
                return 0

            @overload
            def foo(a: str, b):
                """The second overload"""
                return 1

            if random.choice([True, False]):
                a = 1
            else:
                a = "hello"

            foo<CURSOR>(a, 2)
            "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.foo
        ---------------------------------------------
        def foo(
            a: int,
            b
        ) -> Literal[0]
        def foo(
            a: str,
            b
        ) -> Literal[1]
        ---------------------------------------------
        The first overload

        ---------------------------------------------
        function `main.foo`
        ---
        ```python
        def foo(
            a: int,
            b
        ) -> Literal[0]
        def foo(
            a: str,
            b
        ) -> Literal[1]
        ```
        ---
        The first overload
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:19:1
           |
        19 | foo(a, 2)
           | ^^^- Cursor offset
           | |
           | source
        ");
    }

    #[test]
    fn hover_overload_ambiguous_compact() {
        let test = hover_test(
            r#"
            from typing import overload

            @overload
            def foo(a: int):
                """The first overload"""
                return 0

            @overload
            def foo(a: str):
                """The second overload"""
                return 1

            if random.choice([True, False]):
                a = 1
            else:
                a = "hello"

            foo<CURSOR>(a)
            "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.foo
        ---------------------------------------------
        def foo(a: int) -> Literal[0]
        def foo(a: str) -> Literal[1]
        ---------------------------------------------
        The first overload

        ---------------------------------------------
        function `main.foo`
        ---
        ```python
        def foo(a: int) -> Literal[0]
        def foo(a: str) -> Literal[1]
        ```
        ---
        The first overload
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:19:1
           |
        19 | foo(a)
           | ^^^- Cursor offset
           | |
           | source
        ");
    }

    #[test]
    fn hover_module() {
        let mut test = hover_test(
            r#"
            import lib

            li<CURSOR>b
            "#,
        );

        test.write_file(
            "lib.py",
            r"
        '''
        The cool lib_py module!

        Wow this module rocks.
        '''
        a = 10
        ",
        )
        .unwrap();

        assert_snapshot!(test.hover(), @"
        module lib
        ---------------------------------------------
        <module 'lib'>
        ---------------------------------------------
        The cool lib_py module!

        Wow this module rocks.

        ---------------------------------------------
        module `lib`
        ---
        ```xml
        <module 'lib'>
        ```
        ---
        The cool lib/_py module!<HB>
        <HB>
        Wow this module rocks.
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:1
          |
        4 | lib
          | ^^-
          | | |
          | | Cursor offset
          | source
        ");
    }

    #[test]
    fn hover_nonlocal_binding() {
        let test = hover_test(
            r#"
def outer():
    x = "outer_value"

    def inner():
        nonlocal x
        x = "modified"
        return x<CURSOR>  # Should find the nonlocal x declaration in outer scope

    return inner
"#,
        );

        // Should find the variable declaration in the outer scope, not the nonlocal statement
        assert_snapshot!(test.hover(), @r#"
        variable x
        ---------------------------------------------
        Literal["modified"]
        ---------------------------------------------
        variable `x`
        ---
        ```python
        Literal["modified"]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:8:16
          |
        8 |         return x  # Should find the nonlocal x declaration in outer scope
          |                ^- Cursor offset
          |                |
          |                source
        "#);
    }

    #[test]
    fn hover_nonlocal_stmt() {
        let test = hover_test(
            r#"
def outer():
    xy = "outer_value"

    def inner():
        nonlocal x<CURSOR>y
        xy = "modified"
        return x  # Should find the nonlocal x declaration in outer scope

    return inner
"#,
        );

        // Should find the variable declaration in the outer scope, not the nonlocal statement
        assert_snapshot!(test.hover(), @"
        variable xy
        ---------------------------------------------
        variable `xy`
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:6:18
          |
        6 |         nonlocal xy
          |                  ^-
          |                  ||
          |                  |Cursor offset
          |                  source
        ");
    }

    #[test]
    fn hover_global_binding() {
        let test = hover_test(
            r#"
global_var = "global_value"

def function():
    global global_var
    global_var = "modified"
    return global_<CURSOR>var  # Should find the global variable declaration
"#,
        );

        // Should find the global variable declaration, not the global statement
        assert_snapshot!(test.hover(), @r#"
        variable main.global_var
        ---------------------------------------------
        Literal["modified"]
        ---------------------------------------------
        variable `main.global_var`
        ---
        ```python
        Literal["modified"]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:7:12
          |
        7 |     return global_var  # Should find the global variable declaration
          |            ^^^^^^^-^^
          |            |      |
          |            |      Cursor offset
          |            source
        "#);
    }

    #[test]
    fn hover_global_stmt() {
        let test = hover_test(
            r#"
global_var = "global_value"

def function():
    global global_<CURSOR>var
    global_var = "modified"
    return global_var  # Should find the global variable declaration
"#,
        );

        // Should find the global variable declaration, not the global statement
        assert_snapshot!(test.hover(), @"
        variable main.global_var
        ---------------------------------------------
        variable `main.global_var`
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:5:12
          |
        5 |     global global_var
          |            ^^^^^^^-^^
          |            |      |
          |            |      Cursor offset
          |            source
        ");
    }

    #[test]
    fn hover_match_name_stmt() {
        let test = hover_test(
            r#"
            def my_func(command: str):
                match command.split():
                    case ["get", a<CURSOR>b]:
                        x = ab
            "#,
        );

        assert_snapshot!(test.hover(), @r#"
        variable ab
        ---------------------------------------------
        variable `ab`
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:22
          |
        4 |         case ["get", ab]:
          |                      ^-
          |                      ||
          |                      |Cursor offset
          |                      source
        "#);
    }

    #[test]
    fn hover_match_name_binding() {
        let test = hover_test(
            r#"
            def my_func(command: str):
                match command.split():
                    case ["get", ab]:
                        x = a<CURSOR>b
            "#,
        );

        assert_snapshot!(test.hover(), @"
        variable ab
        ---------------------------------------------
        str
        ---------------------------------------------
        variable `ab`
        ---
        ```python
        str
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:5:17
          |
        5 |             x = ab
          |                 ^-
          |                 ||
          |                 |Cursor offset
          |                 source
        ");
    }

    #[test]
    fn hover_match_rest_stmt() {
        let test = hover_test(
            r#"
            def my_func(command: str):
                match command.split():
                    case ["get", *a<CURSOR>b]:
                        x = ab
            "#,
        );

        assert_snapshot!(test.hover(), @r#"
        variable ab
        ---------------------------------------------
        variable `ab`
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:23
          |
        4 |         case ["get", *ab]:
          |                       ^-
          |                       ||
          |                       |Cursor offset
          |                       source
        "#);
    }

    #[test]
    fn hover_match_rest_binding() {
        let test = hover_test(
            r#"
            def my_func(command: str):
                match command.split():
                    case ["get", *ab]:
                        x = a<CURSOR>b
            "#,
        );

        assert_snapshot!(test.hover(), @"
        variable ab
        ---------------------------------------------
        list[str]
        ---------------------------------------------
        variable `ab`
        ---
        ```python
        list[str]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:5:17
          |
        5 |             x = ab
          |                 ^-
          |                 ||
          |                 |Cursor offset
          |                 source
        ");
    }

    #[test]
    fn hover_match_as_stmt() {
        let test = hover_test(
            r#"
            def my_func(command: str):
                match command.split():
                    case ["get", ("a" | "b") as a<CURSOR>b]:
                        x = ab
            "#,
        );

        assert_snapshot!(test.hover(), @r#"
        variable ab
        ---------------------------------------------
        variable `ab`
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:37
          |
        4 |         case ["get", ("a" | "b") as ab]:
          |                                     ^-
          |                                     ||
          |                                     |Cursor offset
          |                                     source
        "#);
    }

    #[test]
    fn hover_match_as_binding() {
        let test = hover_test(
            r#"
            def my_func(command: str):
                match command.split():
                    case ["get", ("a" | "b") as ab]:
                        x = a<CURSOR>b
            "#,
        );

        assert_snapshot!(test.hover(), @r#"
        variable ab
        ---------------------------------------------
        Literal["a", "b"]
        ---------------------------------------------
        variable `ab`
        ---
        ```python
        Literal["a", "b"]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:5:17
          |
        5 |             x = ab
          |                 ^-
          |                 ||
          |                 |Cursor offset
          |                 source
        "#);
    }

    #[test]
    fn hover_match_keyword_stmt() {
        let test = hover_test(
            r#"
            class Click:
                __match_args__ = ("position", "button")
                def __init__(self, pos, btn):
                    self.position: int = pos
                    self.button: str = btn

            def my_func(event: Click):
                match event:
                    case Click(x, button=a<CURSOR>b):
                        x = ab
            "#,
        );

        assert_snapshot!(test.hover(), @"
        variable ab
        ---------------------------------------------
        variable `ab`
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:10:30
           |
        10 |         case Click(x, button=ab):
           |                              ^-
           |                              ||
           |                              |Cursor offset
           |                              source
        ");
    }

    #[test]
    fn hover_match_keyword_binding() {
        let test = hover_test(
            r#"
            class Click:
                __match_args__ = ("position", "button")
                def __init__(self, pos, btn):
                    self.position: int = pos
                    self.button: str = btn

            def my_func(event: Click):
                match event:
                    case Click(x, button=ab):
                        x = a<CURSOR>b
            "#,
        );

        assert_snapshot!(test.hover(), @"
        variable ab
        ---------------------------------------------
        str
        ---------------------------------------------
        variable `ab`
        ---
        ```python
        str
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:11:17
           |
        11 |             x = ab
           |                 ^-
           |                 ||
           |                 |Cursor offset
           |                 source
        ");
    }

    #[test]
    fn hover_match_class_name() {
        let test = hover_test(
            r#"
            class Click:
                __match_args__ = ("position", "button")
                def __init__(self, pos, btn):
                    self.position: int = pos
                    self.button: str = btn

            def my_func(event: Click):
                match event:
                    case Cl<CURSOR>ick(x, button=ab):
                        x = ab
            "#,
        );

        assert_snapshot!(test.hover(), @"
        class main.Click
        ---------------------------------------------
        <class 'Click'>
        ---------------------------------------------
        class `main.Click`
        ---
        ```xml
        <class 'Click'>
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:10:14
           |
        10 |         case Click(x, button=ab):
           |              ^^-^^
           |              | |
           |              | Cursor offset
           |              source
        ");
    }

    #[test]
    fn hover_match_class_field_name() {
        let test = hover_test(
            r#"
            class Click:
                __match_args__ = ("position", "button")
                def __init__(self, pos, btn):
                    self.position: int = pos
                    self.button: str = btn

            def my_func(event: Click):
                match event:
                    case Click(x, but<CURSOR>ton=ab):
                        x = ab
            "#,
        );

        assert_snapshot!(test.hover(), @"Hover provided no content");
    }

    #[test]
    fn hover_typevar_name_stmt() {
        let test = hover_test(
            r#"
            type Alias1[A<CURSOR>B: int = bool] = tuple[AB, list[AB]]
            "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter AB
        ---------------------------------------------
        AB (invariant)
        ---------------------------------------------
        type parameter `AB`
        ---
        ```python
        AB (invariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:13
          |
        2 | type Alias1[AB: int = bool] = tuple[AB, list[AB]]
          |             ^-
          |             ||
          |             |Cursor offset
          |             source
        ");
    }

    #[test]
    fn hover_typevar_name_binding() {
        let test = hover_test(
            r#"
            type Alias1[AB: int = bool] = tuple[A<CURSOR>B, list[AB]]
            "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter AB
        ---------------------------------------------
        AB (invariant)
        ---------------------------------------------
        type parameter `AB`
        ---
        ```python
        AB (invariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:37
          |
        2 | type Alias1[AB: int = bool] = tuple[AB, list[AB]]
          |                                     ^-
          |                                     ||
          |                                     |Cursor offset
          |                                     source
        ");
    }

    #[test]
    fn hover_typevar_spec_stmt() {
        let test = hover_test(
            r#"
            from typing import Callable
            type Alias2[**A<CURSOR>B = [int, str]] = Callable[AB, tuple[AB]]
            "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter AB
        ---------------------------------------------
        AB (contravariant)
        ---------------------------------------------
        type parameter `AB`
        ---
        ```python
        AB (contravariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:15
          |
        3 | type Alias2[**AB = [int, str]] = Callable[AB, tuple[AB]]
          |               ^-
          |               ||
          |               |Cursor offset
          |               source
        ");
    }

    #[test]
    fn hover_typevar_spec_binding() {
        let test = hover_test(
            r#"
            from typing import Callable
            type Alias2[**AB = [int, str]] = Callable[A<CURSOR>B, tuple[AB]]
            "#,
        );

        // TODO: This should just be `**AB@Alias2 (<variance>)`
        // https://github.com/astral-sh/ty/issues/1581
        assert_snapshot!(test.hover(), @"
        type parameter AB
        ---------------------------------------------
        (**AB) -> tuple[Unknown]
        ---------------------------------------------
        type parameter `AB`
        ---
        ```python
        (**AB) -> tuple[Unknown]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:43
          |
        3 | type Alias2[**AB = [int, str]] = Callable[AB, tuple[AB]]
          |                                           ^-
          |                                           ||
          |                                           |Cursor offset
          |                                           source
        ");
    }

    #[test]
    fn hover_typevar_tuple_stmt() {
        let test = hover_test(
            r#"
            type Alias3[*A<CURSOR>B = ()] = tuple[tuple[*AB], tuple[*AB]]
            "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter AB
        ---------------------------------------------
        type parameter `AB`
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:14
          |
        2 | type Alias3[*AB = ()] = tuple[tuple[*AB], tuple[*AB]]
          |              ^-
          |              ||
          |              |Cursor offset
          |              source
        ");
    }

    #[test]
    fn hover_typevar_tuple_binding() {
        let test = hover_test(
            r#"
            type Alias3[*AB = ()] = tuple[tuple[*A<CURSOR>B], tuple[*AB]]
            "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter AB
        ---------------------------------------------
        AB (covariant)
        ---------------------------------------------
        type parameter `AB`
        ---
        ```python
        AB (covariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:38
          |
        2 | type Alias3[*AB = ()] = tuple[tuple[*AB], tuple[*AB]]
          |                                      ^-
          |                                      ||
          |                                      |Cursor offset
          |                                      source
        ");
    }

    #[test]
    fn hover_module_import() {
        let mut test = hover_test(
            r#"
            import li<CURSOR>b

            lib
            "#,
        );

        test.write_file(
            "lib.py",
            r"
        '''
        The cool lib_py module!

        Wow this module rocks.
        '''
        a = 10
        ",
        )
        .unwrap();

        assert_snapshot!(test.hover(), @"
        module lib
        ---------------------------------------------
        <module 'lib'>
        ---------------------------------------------
        The cool lib_py module!

        Wow this module rocks.

        ---------------------------------------------
        module `lib`
        ---
        ```xml
        <module 'lib'>
        ```
        ---
        The cool lib/_py module!<HB>
        <HB>
        Wow this module rocks.
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:8
          |
        2 | import lib
          |        ^^-
          |        | |
          |        | Cursor offset
          |        source
        ");
    }

    /// a decorated method is shown as the wrapper its decorator returns, then as its `def` writes
    /// it, with its own docstring rather than the wrapper's
    #[test]
    fn hover_decorated_method_call() {
        let test = hover_test(
            r#"
            def logged(func):
                def wrapper(*args, **kwargs):
                    """Call the function."""
                    return func(*args, **kwargs)
                return wrapper

            class Circle:
                @logged
                def area(self, scale: int) -> float:
                    """The area of the circle."""
                    return 1.0

            Circle().ar<CURSOR>ea(2)
            "#,
        );

        assert_snapshot!(test.hover(), @"
        method main.Circle.area
        ---------------------------------------------
        def wrapper(...) -> Unknown
        ---------------------------------------------
        @logged
        def area(
            self,
            scale: int
        ) -> int | float
        ---------------------------------------------
        The area of the circle.

        ---------------------------------------------
        method `main.Circle.area`
        ---
        ```python
        def wrapper(...) -> Unknown
        ```
        ---
        ```python
        @logged
        def area(
            self,
            scale: int
        ) -> int | float
        ```
        ---
        The area of the circle.
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:14:10
           |
        14 | Circle().area(2)
           |          ^^-^
           |          | |
           |          | Cursor offset
           |          source
        ");
    }

    /// a `def` whose decorator returns something other than a function is shown as that, then as
    /// the `def` writes it
    #[test]
    fn hover_context_manager_def_use() {
        let test = hover_test(
            r#"
            from contextlib import contextmanager
            from collections.abc import Iterator

            @contextmanager
            def opened() -> Iterator[int]:
                """Open it."""
                yield 1

            open<CURSOR>ed()
            "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.opened
        ---------------------------------------------
        () -> _GeneratorContextManager[int, None, None]
        ---------------------------------------------
        @contextmanager
        def opened() -> Iterator[int]
        ---------------------------------------------
        Open it.

        ---------------------------------------------
        function `main.opened`
        ---
        ```python
        () -> _GeneratorContextManager[int, None, None]
        ```
        ---
        ```python
        @contextmanager
        def opened() -> Iterator[int]
        ```
        ---
        Open it.
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:10:1
           |
        10 | opened()
           | ^^^^-^
           | |   |
           | |   Cursor offset
           | source
        ");
    }

    #[test]
    fn hover_cached_def() {
        let test = hover_test(
            r#"
            import functools

            @functools.cache
            def comp<CURSOR>uted(x: int) -> str:
                return ""
            "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.computed
        ---------------------------------------------
        _lru_cache_wrapper[def computed(x: int) -> str]
        ---------------------------------------------
        @functools.cache
        def computed(x: int) -> str
        ---------------------------------------------
        function `main.computed`
        ---
        ```python
        _lru_cache_wrapper[def computed(x: int) -> str]
        ```
        ---
        ```python
        @functools.cache
        def computed(x: int) -> str
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:5:5
          |
        5 | def computed(x: int) -> str:
          |     ^^^^-^^^
          |     |   |
          |     |   Cursor offset
          |     source
        ");
    }

    /// a decorator that returns the function it is given leaves the name bound to the `def`, so
    /// the hover shows only the `def` as it is written
    #[test]
    fn hover_def_decorated_by_identity_decorator() {
        let test = hover_test(
            r#"
            def registered[F](function: F) -> F:
                return function

            @registered
            def han<CURSOR>dler(x: int) -> str:
                return ""
            "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.handler
        ---------------------------------------------
        @registered
        def handler(x: int) -> str
        ---------------------------------------------
        function `main.handler`
        ---
        ```python
        @registered
        def handler(x: int) -> str
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:6:5
          |
        6 | def handler(x: int) -> str:
          |     ^^^-^^^
          |     |  |
          |     |  Cursor offset
          |     source
        ");
    }

    /// a property's `def` is shown with its decorator, and its kind says it is a property
    #[test]
    fn hover_property_def() {
        let test = hover_test(
            r#"
            class Circle:
                @property
                def diam<CURSOR>eter(self) -> float:
                    return 2.0
            "#,
        );

        assert_snapshot!(test.hover(), @"
        property main.Circle.diameter
        ---------------------------------------------
        property
        ---------------------------------------------
        @property
        def diameter(self) -> int | float
        ---------------------------------------------
        property `main.Circle.diameter`
        ---
        ```python
        property
        ```
        ---
        ```python
        @property
        def diameter(self) -> int | float
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:9
          |
        4 |     def diameter(self) -> float:
          |         ^^^^-^^^
          |         |   |
          |         |   Cursor offset
          |         source
        ");
    }

    #[test]
    fn hover_callable_special_forms() {
        let test = hover_test(
            r#"
            from collections.abc import Callable

            Callable<CURSOR>
            "#,
        );

        let hover = test.hover();
        assert!(
            hover.contains("\n<special-form 'collections.abc.Callable'>"),
            "{hover}"
        );

        let test = hover_test(
            r#"
            from typing import Callable

            Callable<CURSOR>
            "#,
        );

        let hover = test.hover();
        assert!(
            hover.contains("\n<special-form 'typing.Callable'>"),
            "{hover}"
        );
    }

    #[test]
    fn hover_type_of_expression_with_type_var_type() {
        let test = hover_test(
            r#"
            type Alias[T: int = bool] = list[T<CURSOR>]
            "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter T
        ---------------------------------------------
        T (invariant)
        ---------------------------------------------
        type parameter `T`
        ---
        ```python
        T (invariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:34
          |
        2 | type Alias[T: int = bool] = list[T]
          |                                  ^- Cursor offset
          |                                  |
          |                                  source
        ");
    }

    #[test]
    fn hover_type_of_expression_with_type_param_spec() {
        let test = hover_test(
            r#"
            type Alias[**P = [int, str]] = Callable[P<CURSOR>, int]
            "#,
        );

        // TODO: Should this be constravariant instead?
        assert_snapshot!(test.hover(), @"
        type parameter P
        ---------------------------------------------
        P (covariant)
        ---------------------------------------------
        type parameter `P`
        ---
        ```python
        P (covariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:41
          |
        2 | type Alias[**P = [int, str]] = Callable[P, int]
          |                                         ^- Cursor offset
          |                                         |
          |                                         source
        ");
    }

    #[test]
    fn hover_type_of_expression_with_type_var_tuple() {
        let test = hover_test(
            r#"
            type Alias[*Ts = ()] = tuple[*Ts<CURSOR>]
            "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter Ts
        ---------------------------------------------
        Ts (covariant)
        ---------------------------------------------
        type parameter `Ts`
        ---
        ```python
        Ts (covariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:31
          |
        2 | type Alias[*Ts = ()] = tuple[*Ts]
          |                               ^^- Cursor offset
          |                               |
          |                               source
        ");
    }

    #[test]
    fn hover_type_var_in_scope_is_written_by_its_name() {
        let test = hover_test(
            r#"
            def f[T](t: T):
                a<CURSOR> = t
            "#,
        );

        assert_snapshot!(test.hover(), @"
        variable a
        ---------------------------------------------
        T
        ---------------------------------------------
        variable `a`
        ---
        ```python
        T
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:5
          |
        3 |     a = t
          |     ^- Cursor offset
          |     |
          |     source
        ");
    }

    #[test]
    fn hover_parameter_type_var_in_scope_is_written_by_its_name() {
        let test = hover_test(
            r#"
            def f[T](t<CURSOR>: T): ...
            "#,
        );

        assert_snapshot!(test.hover(), @"
        parameter t
        ---------------------------------------------
        T
        ---------------------------------------------
        parameter `t`
        ---
        ```python
        T
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:10
          |
        2 | def f[T](t: T): ...
          |          ^- Cursor offset
          |          |
          |          source
        ");
    }

    #[test]
    fn hover_type_var_in_its_own_signature_is_written_by_its_name() {
        let test = hover_test(
            r#"
            def f[T](t: T<CURSOR>): ...
            "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter T
        ---------------------------------------------
        T
        ---------------------------------------------
        type parameter `T`
        ---
        ```python
        T
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:13
          |
        2 | def f[T](t: T): ...
          |             ^- Cursor offset
          |             |
          |             source
        ");
    }

    #[test]
    fn hover_legacy_parameter_type_var_in_scope_is_written_by_its_name() {
        let test = hover_test(
            r#"
            from typing import TypeVar

            T = TypeVar("T")

            def f(t<CURSOR>: T): ...
            "#,
        );

        assert_snapshot!(test.hover(), @"
        parameter t
        ---------------------------------------------
        T
        ---------------------------------------------
        parameter `t`
        ---
        ```python
        T
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:6:7
          |
        6 | def f(t: T): ...
          |       ^- Cursor offset
          |       |
          |       source
        ");
    }

    #[test]
    fn hover_legacy_type_var_in_scope_is_written_by_its_name() {
        let test = hover_test(
            r#"
            from typing import TypeVar

            T = TypeVar("T")

            def f(t: T):
                a<CURSOR> = t
            "#,
        );

        assert_snapshot!(test.hover(), @"
        variable a
        ---------------------------------------------
        T
        ---------------------------------------------
        variable `a`
        ---
        ```python
        T
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:7:5
          |
        7 |     a = t
          |     ^- Cursor offset
          |     |
          |     source
        ");
    }

    #[test]
    fn hover_type_var_hidden_by_a_nearer_one_keeps_its_scope() {
        let test = hover_test(
            r#"
            def f[T](t: T):
                def g[T](u: T):
                    a<CURSOR> = (t, u)
            "#,
        );

        assert_snapshot!(test.hover(), @"
        variable a
        ---------------------------------------------
        tuple[T@f, T]
        ---------------------------------------------
        variable `a`
        ---
        ```python
        tuple[T@f, T]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:9
          |
        4 |         a = (t, u)
          |         ^- Cursor offset
          |         |
          |         source
        ");
    }

    #[test]
    fn hover_param_spec_component_in_scope_is_written_by_its_name() {
        let test = hover_test(
            r#"
            def f[**P](*args: P.args, **kwargs: P.kwargs):
                a<CURSOR> = args
            "#,
        );

        assert_snapshot!(test.hover(), @"
        variable a
        ---------------------------------------------
        P.args
        ---------------------------------------------
        variable `a`
        ---
        ```python
        P.args
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:5
          |
        3 |     a = args
          |     ^- Cursor offset
          |     |
          |     source
        ");
    }

    #[test]
    fn hover_variable_assignment() {
        let test = hover_test(
            r#"
            value<CURSOR> = 1
            """This is the docs for this value

            Wow these are good docs!
            """
            "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.value
        ---------------------------------------------
        Literal[1]
        ---------------------------------------------
        This is the docs for this value

        Wow these are good docs!

        ---------------------------------------------
        variable `main.value`
        ---
        ```python
        Literal[1]
        ```
        ---
        This is the docs for this value<HB>
        <HB>
        Wow these are good docs!
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:1
          |
        2 | value = 1
          | ^^^^^- Cursor offset
          | |
          | source
        ");
    }

    #[test]
    fn hover_augmented_assignment() {
        let test = hover_test(
            r#"
            value = 1
            """This is the docs for this value

            Wow these are good docs!
            """
            value<CURSOR> += 2
            """Other docs???

            Is this allowed???
            """
            "#,
        );

        // We currently show the *previous* value of the variable (1), not the new one (3).
        // Showing the new value might be more intuitive for some users, but the actual 'use'
        // of the `value` symbol here in read-context is `1`. This comment mainly exists to
        // signal that it might be okay to revisit this in the future and reveal 3 instead.
        assert_snapshot!(test.hover(), @"
        variable main.value
        ---------------------------------------------
        Literal[1]
        ---------------------------------------------
        This is the docs for this value

        Wow these are good docs!

        ---------------------------------------------
        variable `main.value`
        ---
        ```python
        Literal[1]
        ```
        ---
        This is the docs for this value<HB>
        <HB>
        Wow these are good docs!
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:7:1
          |
        7 | value += 2
          | ^^^^^- Cursor offset
          | |
          | source
        ");
    }

    #[test]
    fn hover_attribute_assignment() {
        let test = hover_test(
            r#"
            class C:
                attr: int = 1
                """This is the docs for this value

                Wow these are good docs!
                """

            C.attr<CURSOR> = 2
            """Other docs???

            Is this allowed???
            """
            "#,
        );

        assert_snapshot!(test.hover(), @"
        attribute main.C.attr
        ---------------------------------------------
        Literal[2]
        ---------------------------------------------
        This is the docs for this value

        Wow these are good docs!

        ---------------------------------------------
        attribute `main.C.attr`
        ---
        ```python
        Literal[2]
        ```
        ---
        This is the docs for this value<HB>
        <HB>
        Wow these are good docs!
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:9:3
          |
        9 | C.attr = 2
          |   ^^^^- Cursor offset
          |   |
          |   source
        ");
    }

    #[test]
    fn hover_augmented_attribute_assignment() {
        let test = hover_test(
            r#"
            class C:
                attr = 1
                """This is the docs for this value

                Wow these are good docs!
                """

            C.attr<CURSOR> += 2
            """Other docs???

            Is this allowed???
            """
            "#,
        );

        // See the comment in the `hover_augmented_assignment` test above. The same
        // reasoning applies here.
        assert_snapshot!(test.hover(), @"
        attribute main.C.attr
        ---------------------------------------------
        int
        ---------------------------------------------
        This is the docs for this value

        Wow these are good docs!

        ---------------------------------------------
        attribute `main.C.attr`
        ---
        ```python
        int
        ```
        ---
        This is the docs for this value<HB>
        <HB>
        Wow these are good docs!
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:9:3
          |
        9 | C.attr += 2
          |   ^^^^- Cursor offset
          |   |
          |   source
        ");
    }

    #[test]
    fn hover_annotated_assignment() {
        let test = hover_test(
            r#"
        class Foo:
            a<CURSOR>: int
            """This is the docs for this value

            Wow these are good docs!
            """
        "#,
        );

        assert_snapshot!(test.hover(), @"
        attribute main.Foo.a
        ---------------------------------------------
        int
        ---------------------------------------------
        This is the docs for this value

        Wow these are good docs!

        ---------------------------------------------
        attribute `main.Foo.a`
        ---
        ```python
        int
        ```
        ---
        This is the docs for this value<HB>
        <HB>
        Wow these are good docs!
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:5
          |
        3 |     a: int
          |     ^- Cursor offset
          |     |
          |     source
        ");
    }

    #[test]
    fn hover_annotated_assignment_with_rhs() {
        let test = hover_test(
            r#"
        class Foo:
            a<CURSOR>: int = 1
            """This is the docs for this value

            Wow these are good docs!
            """
        "#,
        );

        assert_snapshot!(test.hover(), @"
        attribute main.Foo.a
        ---------------------------------------------
        Literal[1]
        ---------------------------------------------
        This is the docs for this value

        Wow these are good docs!

        ---------------------------------------------
        attribute `main.Foo.a`
        ---
        ```python
        Literal[1]
        ```
        ---
        This is the docs for this value<HB>
        <HB>
        Wow these are good docs!
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:5
          |
        3 |     a: int = 1
          |     ^- Cursor offset
          |     |
          |     source
        ");
    }

    #[test]
    fn hover_annotated_assignment_with_rhs_use() {
        let test = hover_test(
            r#"
        class Foo:
            a: int = 1
            """This is the docs for this value

            Wow these are good docs!
            """

        x = Foo()
        x.a<CURSOR>
        "#,
        );

        assert_snapshot!(test.hover(), @"
        attribute main.Foo.a
        ---------------------------------------------
        int
        ---------------------------------------------
        This is the docs for this value

        Wow these are good docs!

        ---------------------------------------------
        attribute `main.Foo.a`
        ---
        ```python
        int
        ```
        ---
        This is the docs for this value<HB>
        <HB>
        Wow these are good docs!
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:10:3
           |
        10 | x.a
           |   ^- Cursor offset
           |   |
           |   source
        ");
    }

    #[test]
    fn hover_annotated_attribute_assignment() {
        let test = hover_test(
            r#"
        class Foo:
            def __init__(self, a: int):
                self.a<CURSOR>: int = a
                """This is the docs for this value

                Wow these are good docs!
                """
        "#,
        );

        assert_snapshot!(test.hover(), @"
        attribute main.Foo.a
        ---------------------------------------------
        int
        ---------------------------------------------
        This is the docs for this value

        Wow these are good docs!

        ---------------------------------------------
        attribute `main.Foo.a`
        ---
        ```python
        int
        ```
        ---
        This is the docs for this value<HB>
        <HB>
        Wow these are good docs!
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:14
          |
        4 |         self.a: int = a
          |              ^- Cursor offset
          |              |
          |              source
        ");
    }

    #[test]
    fn hover_annotated_attribute_assignment_use() {
        let test = hover_test(
            r#"
        class Foo:
            def __init__(self, a: int):
                self.a: int = a
                """This is the docs for this value

                Wow these are good docs!
                """

        x = Foo(1)
        x.a<CURSOR>
        "#,
        );

        assert_snapshot!(test.hover(), @"
        attribute main.Foo.a
        ---------------------------------------------
        int
        ---------------------------------------------
        This is the docs for this value

        Wow these are good docs!

        ---------------------------------------------
        attribute `main.Foo.a`
        ---
        ```python
        int
        ```
        ---
        This is the docs for this value<HB>
        <HB>
        Wow these are good docs!
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:11:3
           |
        11 | x.a
           |   ^- Cursor offset
           |   |
           |   source
        ");
    }

    #[test]
    fn hover_bare_final_attribute_assignment() {
        let test = hover_test(
            r#"
        from typing import Final

        class Foo:
            def __init__(self, a: str):
                self.a<CURSOR>: Final = a
        "#,
        );

        assert_snapshot!(test.hover(), @"
        attribute main.Foo.a
        ---------------------------------------------
        str (Final)
        ---------------------------------------------
        attribute `main.Foo.a`
        ---
        ```python
        str (Final)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:6:14
          |
        6 |         self.a: Final = a
          |              ^- Cursor offset
          |              |
          |              source
        ");
    }

    #[test]
    fn hover_final_variable() {
        let test = hover_test(
            r#"
        from typing import Final

        x<CURSOR>: Final[int] = 1
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.x
        ---------------------------------------------
        Literal[1] (Final)
        ---------------------------------------------
        variable `main.x`
        ---
        ```python
        Literal[1] (Final)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:1
          |
        4 | x: Final[int] = 1
          | ^- Cursor offset
          | |
          | source
        ");
    }

    #[test]
    fn hover_final_variable_use() {
        let test = hover_test(
            r#"
        from typing import Final

        x: Final[int] = 1
        print(x<CURSOR>)
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.x
        ---------------------------------------------
        Literal[1] (Final)
        ---------------------------------------------
        variable `main.x`
        ---
        ```python
        Literal[1] (Final)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:5:7
          |
        5 | print(x)
          |       ^- Cursor offset
          |       |
          |       source
        ");
    }

    #[test]
    fn hover_classvar_attribute() {
        let test = hover_test(
            r#"
        from typing import ClassVar

        class Foo:
            x: ClassVar[int] = 1

        obj = Foo()
        obj.x<CURSOR>
        "#,
        );

        assert_snapshot!(test.hover(), @"
        attribute main.Foo.x
        ---------------------------------------------
        int (ClassVar)
        ---------------------------------------------
        attribute `main.Foo.x`
        ---
        ```python
        int (ClassVar)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:8:5
          |
        8 | obj.x
          |     ^- Cursor offset
          |     |
          |     source
        ");
    }

    #[test]
    fn hover_final_global_use() {
        let test = hover_test(
            r#"
        from typing import Final

        x: Final[int] = 1

        def foo():
            global x
            print(x<CURSOR>)
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.x
        ---------------------------------------------
        int (Final)
        ---------------------------------------------
        variable `main.x`
        ---
        ```python
        int (Final)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:8:11
          |
        8 |     print(x)
          |           ^- Cursor offset
          |           |
          |           source
        ");
    }

    #[test]
    fn hover_type_narrowing() {
        let test = hover_test(
            r#"
            def foo(a: str | None, b):
                '''
                    My cool func

                    Args:
                        a: hopefully a string, right?!
                '''
                if a is not None:
                    print(a<CURSOR>)
            "#,
        );

        assert_snapshot!(test.hover(), @"
        parameter a
        ---------------------------------------------
        str
        ---------------------------------------------
        parameter `a`
        ---
        ```python
        str
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:10:15
           |
        10 |         print(a)
           |               ^- Cursor offset
           |               |
           |               source
        ");
    }

    #[test]
    fn hover_whitespace() {
        let test = hover_test(
            r#"
        class C:
            <CURSOR>
            foo: str = 'bar'
        "#,
        );

        assert_snapshot!(test.hover(), @"Hover provided no content");
    }

    #[test]
    fn hover_literal_int() {
        let test = hover_test(
            r#"
        print(
            0 + 1<CURSOR>
        )
        "#,
        );

        assert_snapshot!(test.hover(), @"Hover provided no content");
    }

    #[test]
    fn hover_literal_ellipsis() {
        let test = hover_test(
            r#"
        print(
            .<CURSOR>..
        )
        "#,
        );

        assert_snapshot!(test.hover(), @"Hover provided no content");
    }

    #[test]
    fn hover_subscript_literal_index() {
        let test = hover_test(
            r#"
        values: list[str] = ["a", "b"]
        print(values[0<CURSOR>])
        "#,
        );

        assert_snapshot!(test.hover(), @"
        str
        ---------------------------------------------
        ```python
        str
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:7
          |
        3 | print(values[0])
          |       ^^^^^^^^-
          |       |       |
          |       |       Cursor offset
          |       source
        ");
    }

    #[test]
    fn hover_subscript_literal_index_variants() {
        let cases = [
            r#"
        values: list[str] = ["a", "b"]
        print(values[<CURSOR>0])
        "#,
            r#"
        values: list[str] = ["a", "b"]
        print(values[0<CURSOR>])
        "#,
            r#"
        values: list[str] = ["a", "b"]
        print(values<CURSOR>[0])
        "#,
            r#"
        values: list[str] = ["a", "b"]
        print(values<CURSOR>[-1])
        "#,
            r#"
        values: list[str] = ["a", "b"]
        print(values[<CURSOR>-1])
        "#,
            r#"
        values: list[str] = ["a", "b"]
        print(values[-<CURSOR>1])
        "#,
            r#"
        values: list[str] = ["a", "b"]
        print(values[-1<CURSOR>])
        "#,
            r#"
        values: list[str] = ["a", "b"]
        print(values[+<CURSOR>1])
        "#,
            r#"
        values: list[str] = ["a", "b"]
        print(values[+1<CURSOR>])
        "#,
        ];

        let mut output = String::new();
        for (index, case) in cases.iter().enumerate() {
            let test = hover_test(case);
            let hover = test.hover();
            write!(output, "case {index}:\n{hover}\n\n").unwrap();
        }
        assert_snapshot!(output);
    }

    #[test]
    fn hover_subscript_non_literal_index() {
        let test = hover_test(
            r#"
        values: list[str] = ["a", "b"]
        def get_index() -> int: ...
        idx = get_index()
        print(values[-<CURSOR>idx])
        "#,
        );

        assert_snapshot!(test.hover());
    }

    #[test]
    fn hover_subscript_slice_literal_bounds_list_variants() {
        let list_cases = [
            r#"
        values: list[str] = ["a", "b"]
        values[1<CURSOR>:]
        "#,
            r#"
        values: list[str] = ["a", "b"]
        values[:<CURSOR>-1]
        "#,
            r#"
        values: list[str] = ["a", "b"]
        values[:-<CURSOR>1]
        "#,
            r#"
        values: list[str] = ["a", "b"]
        values[: -1<CURSOR>]
        "#,
            r#"
        values: list[str] = ["a", "b"]
        values[<CURSOR>:2]
        "#,
            r#"
        values: list[str] = ["a", "b"]
        values[:<CURSOR>2]
        "#,
        ];

        let mut output = String::new();
        for (index, case) in list_cases.iter().enumerate() {
            let test = hover_test(case);
            let hover = test.hover();
            write!(output, "list case {index}:\n{hover}\n\n").unwrap();
        }
        assert_snapshot!(output);
    }

    #[test]
    fn hover_subscript_slice_literal_bounds_string_variants() {
        let string_cases = [
            r#"
        def f(s: str):
            s[<CURSOR>1:-1]
        "#,
            r#"
        def f(s: str):
            s[1:<CURSOR>-1]
        "#,
            r#"
        def f(s: str):
            s[1:-<CURSOR>1]
        "#,
        ];

        let mut output = String::new();
        for (index, case) in string_cases.iter().enumerate() {
            let test = hover_test(case);
            let hover = test.hover();
            write!(output, "string case {index}:\n{hover}\n\n").unwrap();
        }
        assert_snapshot!(output);
    }

    #[test]
    fn hover_typed_dict_key_literal() {
        let test = hover_test(
            r#"
        from typing import TypedDict

        class Person(TypedDict):
            """A person in the database"""

            name: str
            """The person's full legal name"""

        person: Person = {"name": "Sarah"}
        person["na<CURSOR>me"]
        "#,
        );

        assert_snapshot!(test.hover(), @r#"
        (key of Person) name: str
        ---------------------------------------------
        The person's full legal name

        ---------------------------------------------
        ```python
        (key of Person) name: str
        ```
        ---
        The person's full legal name
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:11:8
           |
        11 | person["name"]
           |        ^^^-^^
           |        |  |
           |        |  Cursor offset
           |        source
        "#);
    }

    #[test]
    fn hover_complex_type1() {
        let test = hover_test(
            r#"
        from typing import Callable, Any, List
        def ab(x: int, y: Callable[[int, int], Any], z: List[int]) -> int: ...

        a<CURSOR>b
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.ab
        ---------------------------------------------
        def ab(
            x: int,
            y: (int, int, /) -> Any,
            z: list[int]
        ) -> int
        ---------------------------------------------
        function `main.ab`
        ---
        ```python
        def ab(
            x: int,
            y: (int, int, /) -> Any,
            z: list[int]
        ) -> int
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:5:1
          |
        5 | ab
          | ^-
          | ||
          | |Cursor offset
          | source
        ");
    }

    #[test]
    fn hover_complex_type2() {
        let test = hover_test(
            r#"
        from typing import Callable, Tuple, Any
        ab: Tuple[Any, int, Callable[[int, int], Any]] = ...

        a<CURSOR>b
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.ab
        ---------------------------------------------
        tuple[Any, int, (int, int, /) -> Any]
        ---------------------------------------------
        variable `main.ab`
        ---
        ```python
        tuple[Any, int, (int, int, /) -> Any]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:5:1
          |
        5 | ab
          | ^-
          | ||
          | |Cursor offset
          | source
        ");
    }

    #[test]
    fn hover_complex_type3() {
        let test = hover_test(
            r#"
        from typing import Callable, Any
        ab:  Callable[[int, int], Any] | None  = ...

        a<CURSOR>b
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.ab
        ---------------------------------------------
        ((int, int, /) -> Any) | None
        ---------------------------------------------
        variable `main.ab`
        ---
        ```python
        ((int, int, /) -> Any) | None
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:5:1
          |
        5 | ab
          | ^-
          | ||
          | |Cursor offset
          | source
        ");
    }

    #[test]
    fn hover_docstring() {
        let test = hover_test(
            r#"
        def f():
            """Lorem ipsum dolor sit amet.<CURSOR>"""
        "#,
        );

        assert_snapshot!(test.hover(), @"Hover provided no content");
    }

    #[test]
    fn hover_func_with_concat_docstring() {
        let test = hover_test(
            r#"
        def a<CURSOR>b():
            """wow cool docs""" """and docs"""
            return
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.ab
        ---------------------------------------------
        def ab()
        ---------------------------------------------
        wow cool docsand docs

        ---------------------------------------------
        function `main.ab`
        ---
        ```python
        def ab()
        ```
        ---
        wow cool docsand docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:5
          |
        2 | def ab():
          |     ^-
          |     ||
          |     |Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_func_docstring_escapes_html_in_markdown() {
        let test = hover_test(
            r#"
        def url<CURSOR>parse():
            """Parse a URL into components:
            <scheme>://<netloc>/<path>;<params>?<query>#<fragment>
            """
            return
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.urlparse
        ---------------------------------------------
        def urlparse()
        ---------------------------------------------
        Parse a URL into components:
        <scheme>://<netloc>/<path>;<params>?<query>#<fragment>

        ---------------------------------------------
        function `main.urlparse`
        ---
        ```python
        def urlparse()
        ```
        ---
        Parse a URL into components:<HB>
        &lt;scheme&gt;://&lt;netloc&gt;/&lt;path&gt;;&lt;params&gt;?&lt;query&gt;#&lt;fragment&gt;
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:5
          |
        2 | def urlparse():
          |     ^^^-^^^^
          |     |  |
          |     |  Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_func_with_plus_docstring() {
        let test = hover_test(
            r#"
        def a<CURSOR>b():
            """wow cool docs""" + """and docs"""
            return
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.ab
        ---------------------------------------------
        def ab()
        ---------------------------------------------
        function `main.ab`
        ---
        ```python
        def ab()
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:5
          |
        2 | def ab():
          |     ^-
          |     ||
          |     |Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_func_with_slash_docstring() {
        let test = hover_test(
            r#"
        def a<CURSOR>b():
            """wow cool docs""" \
            """and docs"""
            return
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.ab
        ---------------------------------------------
        def ab()
        ---------------------------------------------
        wow cool docsand docs

        ---------------------------------------------
        function `main.ab`
        ---
        ```python
        def ab()
        ```
        ---
        wow cool docsand docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:5
          |
        2 | def ab():
          |     ^-
          |     ||
          |     |Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_func_with_sameline_commented_docstring() {
        let test = hover_test(
            r#"
        def a<CURSOR>b():
            """wow cool docs""" # and a comment
            """and docs"""      # that shouldn't be included
            return
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.ab
        ---------------------------------------------
        def ab()
        ---------------------------------------------
        wow cool docs

        ---------------------------------------------
        function `main.ab`
        ---
        ```python
        def ab()
        ```
        ---
        wow cool docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:5
          |
        2 | def ab():
          |     ^-
          |     ||
          |     |Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_func_with_nextline_commented_docstring() {
        let test = hover_test(
            r#"
        def a<CURSOR>b():
            """wow cool docs"""
            # and a comment that shouldn't be included
            """and docs"""
            return
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.ab
        ---------------------------------------------
        def ab()
        ---------------------------------------------
        wow cool docs

        ---------------------------------------------
        function `main.ab`
        ---
        ```python
        def ab()
        ```
        ---
        wow cool docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:5
          |
        2 | def ab():
          |     ^-
          |     ||
          |     |Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_func_with_parens_docstring() {
        let test = hover_test(
            r#"
        def a<CURSOR>b():
            (
                """wow cool docs"""
                """and docs"""
            )
            return
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.ab
        ---------------------------------------------
        def ab()
        ---------------------------------------------
        wow cool docsand docs

        ---------------------------------------------
        function `main.ab`
        ---
        ```python
        def ab()
        ```
        ---
        wow cool docsand docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:5
          |
        2 | def ab():
          |     ^-
          |     ||
          |     |Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_func_with_nextline_commented_parens_docstring() {
        let test = hover_test(
            r#"
        def a<CURSOR>b():
            (
                """wow cool docs"""
                # and a comment that shouldn't be included
                """and docs"""
            )
            return
        "#,
        );

        assert_snapshot!(test.hover(), @"
        function main.ab
        ---------------------------------------------
        def ab()
        ---------------------------------------------
        wow cool docsand docs

        ---------------------------------------------
        function `main.ab`
        ---
        ```python
        def ab()
        ```
        ---
        wow cool docsand docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:5
          |
        2 | def ab():
          |     ^-
          |     ||
          |     |Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_attribute_docstring_spill() {
        let test = hover_test(
            r#"
        if True:
            a<CURSOR>b = 1
        "this shouldn't be a docstring but also it doesn't matter much"
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.ab
        ---------------------------------------------
        Literal[1]
        ---------------------------------------------
        variable `main.ab`
        ---
        ```python
        Literal[1]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:5
          |
        3 |     ab = 1
          |     ^-
          |     ||
          |     |Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_class_typevar_variance() {
        let test = hover_test(
            r#"
        class Covariant[T<CURSOR>]:
            def get(self) -> T:
                raise ValueError
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter T
        ---------------------------------------------
        T (covariant)
        ---------------------------------------------
        type parameter `T`
        ---
        ```python
        T (covariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:17
          |
        2 | class Covariant[T]:
          |                 ^- Cursor offset
          |                 |
          |                 source
        ");

        let test = hover_test(
            r#"
        class Covariant[T]:
            def get(self) -> T<CURSOR>:
                raise ValueError
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter T
        ---------------------------------------------
        T (covariant)
        ---------------------------------------------
        type parameter `T`
        ---
        ```python
        T (covariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:22
          |
        3 |     def get(self) -> T:
          |                      ^- Cursor offset
          |                      |
          |                      source
        ");

        let test = hover_test(
            r#"
        class Contravariant[T<CURSOR>]:
            def set(self, x: T):
                pass
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter T
        ---------------------------------------------
        T (contravariant)
        ---------------------------------------------
        type parameter `T`
        ---
        ```python
        T (contravariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:21
          |
        2 | class Contravariant[T]:
          |                     ^- Cursor offset
          |                     |
          |                     source
        ");

        let test = hover_test(
            r#"
        class Contravariant[T]:
            def set(self, x: T<CURSOR>):
                pass
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter T
        ---------------------------------------------
        T (contravariant)
        ---------------------------------------------
        type parameter `T`
        ---
        ```python
        T (contravariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:22
          |
        3 |     def set(self, x: T):
          |                      ^- Cursor offset
          |                      |
          |                      source
        ");
    }

    #[test]
    fn hover_method_typevar_variance() {
        let test = hover_test(
            r#"
        class C[T]:
            def m[U](self, t: T<CURSOR>, u: U):
                pass
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter T
        ---------------------------------------------
        T (contravariant)
        ---------------------------------------------
        type parameter `T`
        ---
        ```python
        T (contravariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:23
          |
        3 |     def m[U](self, t: T, u: U):
          |                       ^- Cursor offset
          |                       |
          |                       source
        ");

        let test = hover_test(
            r#"
        class C[T]:
            def m[U](self, t: T, u: U<CURSOR>):
                pass
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter U
        ---------------------------------------------
        U
        ---------------------------------------------
        type parameter `U`
        ---
        ```python
        U
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:29
          |
        3 |     def m[U](self, t: T, u: U):
          |                             ^- Cursor offset
          |                             |
          |                             source
        ");
    }

    #[test]
    fn hover_function_typevar_variance() {
        let test = hover_test(
            r#"
        def covariant[T<CURSOR>]() -> T:
            raise ValueError
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter T
        ---------------------------------------------
        T
        ---------------------------------------------
        type parameter `T`
        ---
        ```python
        T
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:15
          |
        2 | def covariant[T]() -> T:
          |               ^- Cursor offset
          |               |
          |               source
        ");

        let test = hover_test(
            r#"
        def covariant[T]() -> T<CURSOR>:
            raise ValueError
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter T
        ---------------------------------------------
        T
        ---------------------------------------------
        type parameter `T`
        ---
        ```python
        T
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:23
          |
        2 | def covariant[T]() -> T:
          |                       ^- Cursor offset
          |                       |
          |                       source
        ");

        let test = hover_test(
            r#"
        def contravariant[T<CURSOR>](x: T):
            pass
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter T
        ---------------------------------------------
        T
        ---------------------------------------------
        type parameter `T`
        ---
        ```python
        T
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:19
          |
        2 | def contravariant[T](x: T):
          |                   ^- Cursor offset
          |                   |
          |                   source
        ");

        let test = hover_test(
            r#"
        def contravariant[T](x: T<CURSOR>):
            pass
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter T
        ---------------------------------------------
        T
        ---------------------------------------------
        type parameter `T`
        ---
        ```python
        T
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:25
          |
        2 | def contravariant[T](x: T):
          |                         ^- Cursor offset
          |                         |
          |                         source
        ");
    }

    #[test]
    fn hover_type_alias_typevar_variance() {
        let test = hover_test(
            r#"
        type List[T<CURSOR>] = list[T]
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter T
        ---------------------------------------------
        T (invariant)
        ---------------------------------------------
        type parameter `T`
        ---
        ```python
        T (invariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:11
          |
        2 | type List[T] = list[T]
          |           ^- Cursor offset
          |           |
          |           source
        ");

        let test = hover_test(
            r#"
        type List[T] = list[T<CURSOR>]
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter T
        ---------------------------------------------
        T (invariant)
        ---------------------------------------------
        type parameter `T`
        ---
        ```python
        T (invariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:21
          |
        2 | type List[T] = list[T]
          |                     ^- Cursor offset
          |                     |
          |                     source
        ");

        let test = hover_test(
            r#"
        type Tuple[T<CURSOR>] = tuple[T]
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter T
        ---------------------------------------------
        T (covariant)
        ---------------------------------------------
        type parameter `T`
        ---
        ```python
        T (covariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:12
          |
        2 | type Tuple[T] = tuple[T]
          |            ^- Cursor offset
          |            |
          |            source
        ");

        let test = hover_test(
            r#"
        type Tuple[T] = tuple[T<CURSOR>]
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type parameter T
        ---------------------------------------------
        T (covariant)
        ---------------------------------------------
        type parameter `T`
        ---
        ```python
        T (covariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:23
          |
        2 | type Tuple[T] = tuple[T]
          |                       ^- Cursor offset
          |                       |
          |                       source
        ");
    }

    #[test]
    fn hover_type_alias_name() {
        let test = hover_test(
            r#"
        type Box<CURSOR> = int | None
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type alias main.Box
        ---------------------------------------------
        type Box = int | None
        ---------------------------------------------
        type alias `main.Box`
        ---
        ```python
        type Box = int | None
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:6
          |
        2 | type Box = int | None
          |      ^^^- Cursor offset
          |      |
          |      source
        ");
    }

    #[test]
    fn hover_implicit_recursive_alias_annotation() {
        let test = hover_test(
            r#"
        Tree = int | list["Tree"]
        def consume(value: Tr<CURSOR>ee): ...
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.Tree
        ---------------------------------------------
        Tree = int | list[Tree]
        ---------------------------------------------
        variable `main.Tree`
        ---
        ```python
        Tree = int | list[Tree]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:20
          |
        3 | def consume(value: Tree): ...
          |                    ^^-^
          |                    | |
          |                    | Cursor offset
          |                    source
        ");
    }

    #[test]
    fn hover_implicit_recursive_alias_value() {
        let test = hover_test(
            r#"
        Tree = int | list["Tree"]
        def consume(value: Tree):
            <CURSOR>value
        "#,
        );

        assert_snapshot!(test.hover(), @"
        parameter value
        ---------------------------------------------
        Tree = int | list[Tree]
        ---------------------------------------------
        parameter `value`
        ---
        ```python
        Tree = int | list[Tree]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:5
          |
        4 |     value
          |     -^^^^
          |     |
          |     source
          |     Cursor offset
        ");
    }

    #[test]
    fn hover_implicit_recursive_alias_generic() {
        let test = hover_test(
            r#"
        from typing import TypeVar
        T = TypeVar("T")
        Tree = T | list["Tree[T]"]
        def consume(value: Tree[int]):
            <CURSOR>value
        "#,
        );

        assert_snapshot!(test.hover(), @"
        parameter value
        ---------------------------------------------
        Tree = T | list[Tree[T]]
        ---------------------------------------------
        parameter `value`
        ---
        ```python
        Tree = T | list[Tree[T]]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:6:5
          |
        6 |     value
          |     -^^^^
          |     |
          |     source
          |     Cursor offset
        ");
    }

    #[test]
    fn hover_implicit_recursive_alias_growing() {
        let test = hover_test(
            r#"
        from typing import TypeVar
        T = TypeVar("T")
        Tree = T | list["Tree[list[T]]"]
        def consume(value: Tree[int]):
            <CURSOR>value
        "#,
        );

        assert_snapshot!(test.hover(), @"
        parameter value
        ---------------------------------------------
        Tree = T | list[Tree[list[T]]]
        ---------------------------------------------
        parameter `value`
        ---
        ```python
        Tree = T | list[Tree[list[T]]]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:6:5
          |
        6 |     value
          |     -^^^^
          |     |
          |     source
          |     Cursor offset
        ");
    }

    #[test]
    fn hover_implicit_recursive_alias_docstring() {
        let test = hover_test(
            r#"
        class Link[T]:
            """A linked value."""
        Tree = Link["Tree"]
        def consume(value: Tree):
            <CURSOR>value
        "#,
        );

        assert_snapshot!(test.hover(), @"
        parameter value
        ---------------------------------------------
        Tree = Link[Tree]
        ---------------------------------------------
        A linked value.

        ---------------------------------------------
        parameter `value`
        ---
        ```python
        Tree = Link[Tree]
        ```
        ---
        A linked value.
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:6:5
          |
        6 |     value
          |     -^^^^
          |     |
          |     source
          |     Cursor offset
        ");
    }

    #[test]
    fn hover_generic_type_alias_name() {
        let test = hover_test(
            r#"
        type Wrapper<CURSOR>[T] = list[T]
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type alias main.Wrapper
        ---------------------------------------------
        type Wrapper[T] = list[T]
        ---------------------------------------------
        Built-in mutable sequence.

        If no argument is given, the constructor creates a new empty list.
        The argument must be an iterable if specified.

        ---------------------------------------------
        type alias `main.Wrapper`
        ---
        ```python
        type Wrapper[T] = list[T]
        ```
        ---
        Built-in mutable sequence.<HB>
        <HB>
        If no argument is given, the constructor creates a new empty list.<HB>
        The argument must be an iterable if specified.
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:6
          |
        2 | type Wrapper[T] = list[T]
          |      ^^^^^^^- Cursor offset
          |      |
          |      source
        ");

        let test = hover_test(
            r#"
        type Box[T] = T | None
        x: Box<CURSOR>[int]
        "#,
        );

        assert_snapshot!(test.hover(), @"
        type Box[T] = T | None
        ---------------------------------------------
        ```python
        type Box[T] = T | None
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:4
          |
        3 | x: Box[int]
          |    ^^^-^^^^
          |    |  |
          |    |  Cursor offset
          |    source
        ");
    }

    #[test]
    fn hover_legacy_typevar_variance() {
        let test = hover_test(
            r#"
        from typing import TypeVar

        T<CURSOR> = TypeVar('T', covariant=True)

        def covariant() -> T:
            raise ValueError
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.T
        ---------------------------------------------
        TypeVar
        ---------------------------------------------
        variable `main.T`
        ---
        ```python
        TypeVar
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:1
          |
        4 | T = TypeVar('T', covariant=True)
          | ^- Cursor offset
          | |
          | source
        ");

        let test = hover_test(
            r#"
        from typing import TypeVar

        T = TypeVar('T', covariant=True)

        def covariant() -> T<CURSOR>:
            raise ValueError
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.T
        ---------------------------------------------
        T@covariant
        ---------------------------------------------
        variable `main.T`
        ---
        ```python
        T@covariant
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:6:20
          |
        6 | def covariant() -> T:
          |                    ^- Cursor offset
          |                    |
          |                    source
        ");

        let test = hover_test(
            r#"
        from typing import TypeVar

        T<CURSOR> = TypeVar('T', contravariant=True)

        def contravariant(x: T):
            pass
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.T
        ---------------------------------------------
        TypeVar
        ---------------------------------------------
        variable `main.T`
        ---
        ```python
        TypeVar
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:1
          |
        4 | T = TypeVar('T', contravariant=True)
          | ^- Cursor offset
          | |
          | source
        ");

        let test = hover_test(
            r#"
        from typing import TypeVar

        T = TypeVar('T', contravariant=True)

        def contravariant(x: T<CURSOR>):
            pass
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.T
        ---------------------------------------------
        T@contravariant
        ---------------------------------------------
        variable `main.T`
        ---
        ```python
        T@contravariant
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:6:22
          |
        6 | def contravariant(x: T):
          |                      ^- Cursor offset
          |                      |
          |                      source
        ");

        let test = hover_test(
            r#"
        from typing import Generic, TypeVar

        T = TypeVar('T', contravariant=True)

        class Contravariant(Generic[T]):
            def m(self, x: T<CURSOR>):
                pass
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.T
        ---------------------------------------------
        T (contravariant)
        ---------------------------------------------
        variable `main.T`
        ---
        ```python
        T (contravariant)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:7:20
          |
        7 |     def m(self, x: T):
          |                    ^- Cursor offset
          |                    |
          |                    source
        ");
    }

    #[test]
    fn hover_binary_operator_literal() {
        let test = hover_test(
            r#"
        result = 5 <CURSOR>+ 3
        "#,
        );

        assert_snapshot!(test.hover(), @"
        method builtins.int.__add__
        ---------------------------------------------
        bound method int.__add__(value: int, /) -> int
        ---------------------------------------------
        Return self+value.

        ---------------------------------------------
        method `builtins.int.__add__`
        ---
        ```python
        bound method int.__add__(value: int, /) -> int
        ```
        ---
        Return self+value.
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:12
          |
        2 | result = 5 + 3
          |            ^
          |            |
          |            source
          |            Cursor offset
        ");
    }

    // An intersection operator in a quoted PEP 613 type alias value shows the Intersection documentation.
    #[test]
    fn hover_intersection_in_type_alias() {
        let test = hover_test(
            r#"
from typing import TypeAlias
Alias: TypeAlias = "int <CURSOR>& str"
"#,
        );
        assert_snapshot!(test.hover(), @r#"
        <special-form 'ty_extensions.Intersection'>
        ---------------------------------------------
        `Intersection[T1, T2, ..., Tn]`, also spelled as `T1 & T2 & ... & Tn`, represents an intersection
        type: the set of all objects that inhabit all of the types `T1`, `T2`, ..., `Tn`.

        For any two fully static types `T1` and `T2`, `Intersection[T1, T2]` is a subtype of both `T1` and
        `T2`. For any type `T3` that is a subtype of both `T1` and `T2`, `Intersection[T1, T2]` is a
        supertype of `T3`.

        In the following example, although neither `P` nor `Q` is a subtype of the other, an instance of `S`
        inhabits `Intersection[P, Q]` because `S` inherits from both `P` and `Q`:

        ```python
        class P: ...
        class Q: ...
        class S(P, Q): ...

        s: Intersection[P, Q] = S()
        ```

        In type expressions, ty supports the experimental spelling `T1 & T2`, and also uses this syntax
        when displaying intersection types. Note that either spelling will fail in runtime contexts or in
        cases where an annotation expression could be evaluated.

        ---------------------------------------------
        ```xml
        <special-form 'ty_extensions.Intersection'>
        ```
        ---
        `Intersection[T1, T2, ..., Tn]`, also spelled as `T1 & T2 & ... & Tn`, represents an intersection<HB>
        type: the set of all objects that inhabit all of the types `T1`, `T2`, ..., `Tn`.<HB>
        <HB>
        For any two fully static types `T1` and `T2`, `Intersection[T1, T2]` is a subtype of both `T1` and<HB>
        `T2`. For any type `T3` that is a subtype of both `T1` and `T2`, `Intersection[T1, T2]` is a<HB>
        supertype of `T3`.<HB>
        <HB>
        In the following example, although neither `P` nor `Q` is a subtype of the other, an instance of `S`<HB>
        inhabits `Intersection[P, Q]` because `S` inherits from both `P` and `Q`:<HB>
        <HB>
        ```python
        class P: ...
        class Q: ...
        class S(P, Q): ...

        s: Intersection[P, Q] = S()
        ```<HB>
        <HB>
        In type expressions, ty supports the experimental spelling `T1 & T2`, and also uses this syntax<HB>
        when displaying intersection types. Note that either spelling will fail in runtime contexts or in<HB>
        cases where an annotation expression could be evaluated.
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:25
          |
        3 | Alias: TypeAlias = "int & str"
          |                         ^
          |                         |
          |                         source
          |                         Cursor offset
        "#);
    }

    // A union operator in a type parameter default shows the Union documentation.
    #[test]
    fn hover_union_in_type_parameter_default() {
        let test = hover_test("class C[T = int <CURSOR>| str]: ...");
        assert_snapshot!(test.hover(), @"
        <special-form 'typing.Union'>
        ---------------------------------------------
        Represent a union type

        E.g. for int | str

        ---------------------------------------------
        ```xml
        <special-form 'typing.Union'>
        ```
        ---
        Represent a union type<HB>
        <HB>
        E.g. for int | str
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:1:17
          |
        1 | class C[T = int | str]: ...
          |                 ^
          |                 |
          |                 source
          |                 Cursor offset
        ");
    }

    // A negation operator nested in two string annotations shows the Not documentation.
    #[test]
    fn hover_negation_in_string_type_alias() {
        let test = hover_test("type Alias = \"'<CURSOR>~int'\"");
        assert_snapshot!(test.hover(), @r#"
        <special-form 'ty_extensions.Not'>
        ---------------------------------------------
        `Not[T]`, also spelled as `~T`, represents the set of all objects that do not inhabit the type `T`.

        In type expressions, ty supports the experimental spelling `~T`, and also uses it when displaying
        negation types. Note that either spelling will fail in runtime contexts or in cases where an
        annotation expression could be evaluated.

        ---------------------------------------------
        ```xml
        <special-form 'ty_extensions.Not'>
        ```
        ---
        `Not[T]`, also spelled as `~T`, represents the set of all objects that do not inhabit the type `T`.<HB>
        <HB>
        In type expressions, ty supports the experimental spelling `~T`, and also uses it when displaying<HB>
        negation types. Note that either spelling will fail in runtime contexts or in cases where an<HB>
        annotation expression could be evaluated.
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:1:16
          |
        1 | type Alias = "'~int'"
          |                ^
          |                |
          |                source
          |                Cursor offset
        "#);
    }

    // An operator in a parameter default shows its normal operation method.
    #[test]
    fn hover_operator_in_parameter_default() {
        let test = hover_test("def f(x: int = 1 <CURSOR>& 2): ...");
        let rendered = test.hover();
        assert!(rendered.contains("__and__"), "{rendered}");
    }

    // A locally defined `TypeAlias` name does not mark an assignment as a type alias.
    #[test]
    fn hover_operator_in_annotated_assignment_value() {
        let test = hover_test("TypeAlias = int\nvalue: TypeAlias = 1 <CURSOR>| 2");
        let rendered = test.hover();
        assert!(rendered.contains("__or__"), "{rendered}");
    }

    // The plain-text and Markdown hovers show the special form and its documentation, with a range
    // covering the operator.
    #[test]
    fn hover_negation_operator_presentation() {
        let test = hover_test("def f(x: <CURSOR>~int): ...");
        assert_snapshot!(test.hover(), @"
        <special-form 'ty_extensions.Not'>
        ---------------------------------------------
        `Not[T]`, also spelled as `~T`, represents the set of all objects that do not inhabit the type `T`.

        In type expressions, ty supports the experimental spelling `~T`, and also uses it when displaying
        negation types. Note that either spelling will fail in runtime contexts or in cases where an
        annotation expression could be evaluated.

        ---------------------------------------------
        ```xml
        <special-form 'ty_extensions.Not'>
        ```
        ---
        `Not[T]`, also spelled as `~T`, represents the set of all objects that do not inhabit the type `T`.<HB>
        <HB>
        In type expressions, ty supports the experimental spelling `~T`, and also uses it when displaying<HB>
        negation types. Note that either spelling will fail in runtime contexts or in cases where an<HB>
        annotation expression could be evaluated.
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:1:10
          |
        1 | def f(x: ~int): ...
          |          ^
          |          |
          |          source
          |          Cursor offset
        ");
    }

    #[test]
    fn hover_binary_operator_overload() {
        let test = hover_test(
            r#"
            from __future__ import annotations
            from typing import overload

            class Test:
                @overload
                def __add__(self, other: Test, /) -> Test:  ...
                @overload
                def __add__(self, other: Other, /) -> Test: ...
                def __add__(self, other: Test | Other, /) -> Test:
                    return self

            class Other: ...

            Test() <CURSOR>+ Test()
        "#,
        );

        // TODO: We should only show the matching overload here.
        // https://github.com/astral-sh/ty/issues/73
        assert_snapshot!(test.hover(), @"
        method main.Test.__add__
        ---------------------------------------------
        def __add__(other: Test, /) -> Test
        def __add__(other: Other, /) -> Test
        ---------------------------------------------
        method `main.Test.__add__`
        ---
        ```python
        def __add__(other: Test, /) -> Test
        def __add__(other: Other, /) -> Test
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:15:8
           |
        15 | Test() + Test()
           |        ^
           |        |
           |        source
           |        Cursor offset
        ");
    }

    #[test]
    fn hover_binary_operator_union() {
        let test = hover_test(
            r#"
            from __future__ import annotations

            class Test:
                def __add__(self, other: Other, /) -> Other:
                    return other

            class Other:
                def __add__(self, other: Other, /) -> Other:
                    return self

            def _(a: Test | Other):
                a +<CURSOR> Other()
        "#,
        );

        assert_snapshot!(test.hover(), @"
        (bound method Test.__add__(other: Other, /) -> Other) | (bound method Other.__add__(other: Other, /) -> Other)
        ---------------------------------------------
        ```python
        (bound method Test.__add__(other: Other, /) -> Other) | (bound method Other.__add__(other: Other, /) -> Other)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
          --> main.py:13:7
           |
        13 |     a + Other()
           |       ^- Cursor offset
           |       |
           |       source
        ");
    }

    #[test]
    fn hover_float_annotation() {
        let test = hover_test(
            r#"
            a: float<CURSOR> = 3.14
        "#,
        );

        assert_snapshot!(test.hover(), @"
        int | float
        ---------------------------------------------
        Convert a string or number to a floating-point number, if possible.

        ---------------------------------------------
        ```python
        int | float
        ```
        ---
        Convert a string or number to a floating-point number, if possible.
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:4
          |
        2 | a: float = 3.14
          |    ^^^^^- Cursor offset
          |    |
          |    source
        ");
    }

    #[test]
    fn hover_shadowed_numeric_builtin() {
        let test = hover_test(
            r#"
            import builtins

            class float: ...

            def f(x: builtins.float | float):
                x<CURSOR>
            "#,
        );

        assert_snapshot!(test.hover());
    }

    #[test]
    fn hover_shadowed_numeric_builtin_in_selected_signature() {
        let test = hover_test(
            r#"
            import builtins
            from typing import overload

            class float: ...

            @overload
            def choose(value: builtins.float | float) -> None: ...
            @overload
            def choose(value: str) -> None: ...
            def choose(value: object) -> None: ...

            choose<CURSOR>(1.0)
            "#,
        );

        assert_snapshot!(test.hover());
    }

    #[test]
    fn hover_shadowed_numeric_builtin_in_keyword_parameter() {
        let test = hover_test(
            r#"
            import builtins

            class float: ...

            def choose(*, value: builtins.float | float) -> None: ...

            choose(value<CURSOR>=1.0)
            "#,
        );

        assert_snapshot!(test.hover());
    }

    #[test]
    fn hover_bare_final_annotation() {
        let test = hover_test(
            r#"
            from typing import Final

            x: Final<CURSOR> = 1
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable typing.Final
        ---------------------------------------------
        <special-form 'typing.Final'>
        ---------------------------------------------
        Special typing construct to indicate final names to type checkers.

        A final name cannot be re-assigned or overridden in a subclass.

        For example::

            MAX_SIZE: Final = 9000
            MAX_SIZE += 1  # Error reported by type checker

            class Connection:
                TIMEOUT: Final[int] = 10

            class FastConnector(Connection):
                TIMEOUT = 1  # Error reported by type checker

        There is no runtime checking of these properties.

        ---------------------------------------------
        variable `typing.Final`
        ---
        ```xml
        <special-form 'typing.Final'>
        ```
        ---
        Special typing construct to indicate final names to type checkers.<HB>
        <HB>
        A final name cannot be re-assigned or overridden in a subclass.<HB>
        <HB>
        For example:  <HB>
        ```````````python
            MAX_SIZE: Final = 9000
            MAX_SIZE += 1  # Error reported by type checker

            class Connection:
                TIMEOUT: Final[int] = 10

            class FastConnector(Connection):
                TIMEOUT = 1  # Error reported by type checker

        ```````````
        There is no runtime checking of these properties.
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:4
          |
        4 | x: Final = 1
          |    ^^^^^- Cursor offset
          |    |
          |    source
        ");

        let test = hover_test(
            r#"
            from typing import Final

            x<CURSOR>: Final = 1
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable main.x
        ---------------------------------------------
        Literal[1] (Final)
        ---------------------------------------------
        variable `main.x`
        ---
        ```python
        Literal[1] (Final)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:4:1
          |
        4 | x: Final = 1
          | ^- Cursor offset
          | |
          | source
        ");
    }

    #[test]
    fn hover_comprehension_type_context() {
        let test = hover_test(
            r#"
            a = [[n]<CURSOR> for n in [1, 2, 3]]
        "#,
        );

        assert_snapshot!(test.hover(), @"
        list[int]
        ---------------------------------------------
        ```python
        list[int]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:6
          |
        2 | a = [[n] for n in [1, 2, 3]]
          |      ^^^- Cursor offset
          |      |
          |      source
        ");

        let test = hover_test(
            r#"
            a: list[list[int | str]] = [[n]<CURSOR> for n in [1, 2, 3]]
        "#,
        );

        assert_snapshot!(test.hover(), @"
        list[int | str]
        ---------------------------------------------
        ```python
        list[int | str]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:29
          |
        2 | a: list[list[int | str]] = [[n] for n in [1, 2, 3]]
          |                             ^^^- Cursor offset
          |                             |
          |                             source
        ");
    }

    #[test]
    fn hover_multi_inference() {
        let test = hover_test(
            r#"
            def list1[T](x: T) -> list[T]:
                return [x]

            def f(x: int, y: int) -> list[int] | list[str]:
                return list1(x<CURSOR> + y)
        "#,
        );

        assert_snapshot!(test.hover(), @"
        parameter x
        ---------------------------------------------
        int
        ---------------------------------------------
        parameter `x`
        ---
        ```python
        int
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:6:18
          |
        6 |     return list1(x + y)
          |                  ^- Cursor offset
          |                  |
          |                  source
        ");

        let test = hover_test(
            r#"
            def f(x: int, y: int) -> list[int] | list[str]:
                return [x<CURSOR> + y]
        "#,
        );

        assert_snapshot!(test.hover(), @"
        parameter x
        ---------------------------------------------
        int
        ---------------------------------------------
        parameter `x`
        ---
        ```python
        int
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:13
          |
        3 |     return [x + y]
          |             ^- Cursor offset
          |             |
          |             source
        ");

        let test = hover_test(
            r#"
            def list1[T](x: T) -> list[T]:
                return [x]

            def f(x: int, y: int) -> list[int] | list[str]:
                return (_<CURSOR> := list1(x + y))
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable _
        ---------------------------------------------
        list[int]
        ---------------------------------------------
        variable `_`
        ---
        ```python
        list[int]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:6:13
          |
        6 |     return (_ := list1(x + y))
          |             ^- Cursor offset
          |             |
          |             source
        ");

        let test = hover_test(
            r#"
            def f(x: int, y: int) -> list[int] | list[str]:
                return (_<CURSOR> := [x + y])
        "#,
        );

        assert_snapshot!(test.hover(), @"
        variable _
        ---------------------------------------------
        list[int]
        ---------------------------------------------
        variable `_`
        ---
        ```python
        list[int]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:13
          |
        3 |     return (_ := [x + y])
          |             ^- Cursor offset
          |             |
          |             source
        ");
    }

    #[test]
    fn hover_submodule_import_from_use() {
        let test = CursorTest::builder()
            .source(
                "mypackage/__init__.py",
                r#"
                from .subpkg.submod import val

                x = sub<CURSOR>pkg
                "#,
            )
            .source("mypackage/subpkg/__init__.py", r#""#)
            .source(
                "mypackage/subpkg/submod.py",
                r#"
                val: int = 0
                "#,
            )
            .build();

        // The module is correct
        assert_snapshot!(test.hover(), @"
        <module 'mypackage.subpkg'>
        ---------------------------------------------
        ```xml
        <module 'mypackage.subpkg'>
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> mypackage/__init__.py:4:5
          |
        4 | x = subpkg
          |     ^^^-^^
          |     |  |
          |     |  Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_submodule_import_from_def() {
        let test = CursorTest::builder()
            .source(
                "mypackage/__init__.py",
                r#"
                from .sub<CURSOR>pkg.submod import val

                x = subpkg
                "#,
            )
            .source("mypackage/subpkg/__init__.py", r#""#)
            .source(
                "mypackage/subpkg/submod.py",
                r#"
                val: int = 0
                "#,
            )
            .build();

        // The module is correct
        assert_snapshot!(test.hover(), @"
        module mypackage.subpkg
        ---------------------------------------------
        <module 'mypackage.subpkg'>
        ---------------------------------------------
        module `mypackage.subpkg`
        ---
        ```xml
        <module 'mypackage.subpkg'>
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> mypackage/__init__.py:2:7
          |
        2 | from .subpkg.submod import val
          |       ^^^-^^
          |       |  |
          |       |  Cursor offset
          |       source
        ");
    }

    #[test]
    fn hover_submodule_import_from_wrong_use() {
        let test = CursorTest::builder()
            .source(
                "mypackage/__init__.py",
                r#"
                from .subpkg.submod import val

                x = sub<CURSOR>mod
                "#,
            )
            .source("mypackage/subpkg/__init__.py", r#""#)
            .source(
                "mypackage/subpkg/submod.py",
                r#"
                val: int = 0
                "#,
            )
            .build();

        // Unknown is correct
        assert_snapshot!(test.hover(), @"
        Unknown
        ---------------------------------------------
        ```python
        Unknown
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> mypackage/__init__.py:4:5
          |
        4 | x = submod
          |     ^^^-^^
          |     |  |
          |     |  Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_submodule_import_from_wrong_def() {
        let test = CursorTest::builder()
            .source(
                "mypackage/__init__.py",
                r#"
                from .subpkg.sub<CURSOR>mod import val

                x = submod
                "#,
            )
            .source("mypackage/subpkg/__init__.py", r#""#)
            .source(
                "mypackage/subpkg/submod.py",
                r#"
                val: int = 0
                "#,
            )
            .build();

        // The submodule is correct
        assert_snapshot!(test.hover(), @"
        module mypackage.subpkg.submod
        ---------------------------------------------
        <module 'mypackage.subpkg.submod'>
        ---------------------------------------------
        module `mypackage.subpkg.submod`
        ---
        ```xml
        <module 'mypackage.subpkg.submod'>
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> mypackage/__init__.py:2:14
          |
        2 | from .subpkg.submod import val
          |              ^^^-^^
          |              |  |
          |              |  Cursor offset
          |              source
        ");
    }

    #[test]
    fn hover_submodule_import_from_confusing_shadowed_def() {
        let test = CursorTest::builder()
            .source(
                "mypackage/__init__.py",
                r#"
                from .sub<CURSOR>pkg import subpkg

                x = subpkg
                "#,
            )
            .source(
                "mypackage/subpkg/__init__.py",
                r#"
                subpkg: int = 10
                "#,
            )
            .build();

        // The module is correct
        assert_snapshot!(test.hover(), @"
        module mypackage.subpkg
        ---------------------------------------------
        <module 'mypackage.subpkg'>
        ---------------------------------------------
        module `mypackage.subpkg`
        ---
        ```xml
        <module 'mypackage.subpkg'>
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> mypackage/__init__.py:2:7
          |
        2 | from .subpkg import subpkg
          |       ^^^-^^
          |       |  |
          |       |  Cursor offset
          |       source
        ");
    }

    #[test]
    fn hover_submodule_import_from_confusing_real_def() {
        let test = CursorTest::builder()
            .source(
                "mypackage/__init__.py",
                r#"
                from .subpkg import sub<CURSOR>pkg

                x = subpkg
                "#,
            )
            .source(
                "mypackage/subpkg/__init__.py",
                r#"
                subpkg: int = 10
                "#,
            )
            .build();

        // int is correct
        assert_snapshot!(test.hover(), @"
        variable mypackage.subpkg.subpkg
        ---------------------------------------------
        int
        ---------------------------------------------
        variable `mypackage.subpkg.subpkg`
        ---
        ```python
        int
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> mypackage/__init__.py:2:21
          |
        2 | from .subpkg import subpkg
          |                     ^^^-^^
          |                     |  |
          |                     |  Cursor offset
          |                     source
        ");
    }

    #[test]
    fn hover_submodule_import_from_confusing_use() {
        let test = CursorTest::builder()
            .source(
                "mypackage/__init__.py",
                r#"
                from .subpkg import subpkg

                x = sub<CURSOR>pkg
                "#,
            )
            .source(
                "mypackage/subpkg/__init__.py",
                r#"
                subpkg: int = 10
                "#,
            )
            .build();

        // int is correct
        assert_snapshot!(test.hover(), @"
        variable mypackage.subpkg.subpkg
        ---------------------------------------------
        int
        ---------------------------------------------
        variable `mypackage.subpkg.subpkg`
        ---
        ```python
        int
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> mypackage/__init__.py:4:5
          |
        4 | x = subpkg
          |     ^^^-^^
          |     |  |
          |     |  Cursor offset
          |     source
        ");
    }

    #[test]
    fn hover_tuple_assignment_target() {
        let test = CursorTest::builder()
            .source(
                "test.py",
                r#"
                (x, y)<CURSOR> = "test", 10
                "#,
            )
            .build();

        assert_snapshot!(test.hover(), @"Hover provided no content");
    }

    #[test]
    fn hover_named_expression_target() {
        let test = CursorTest::builder()
            .source(
                "mymod.py",
                r#"
                if a<CURSOR> := 10:
                    pass
                "#,
            )
            .build();

        assert_snapshot!(test.hover(), @"
        variable mymod.a
        ---------------------------------------------
        Literal[10]
        ---------------------------------------------
        variable `mymod.a`
        ---
        ```python
        Literal[10]
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> mymod.py:2:4
          |
        2 | if a := 10:
          |    ^- Cursor offset
          |    |
          |    source
        ");
    }

    /// a hover is read as source, so a basedpython file spells the type the way
    /// it would be written there: `1`, not `Literal[1]`
    #[test]
    fn hover_basedpython_literal() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                r#"
                a<CURSOR> = 1
                "#,
            )
            .build();

        assert_snapshot!(test.hover(), @"
        variable main.a
        ---------------------------------------------
        1
        ---------------------------------------------
        variable `main.a`
        ---
        ```python
        1
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.by:2:1
          |
        2 | a = 1
          | ^- Cursor offset
          | |
          | source
        ");
    }

    /// the signature a hover shows is rendered up front rather than at display
    /// time, so it needs the file's spelling too
    #[test]
    fn hover_basedpython_call_signature() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                r#"
                from typing import Literal

                def f(x: Literal[1]) -> None:
                    return None

                f<CURSOR>(1)
                "#,
            )
            .build();

        assert_snapshot!(test.hover(), @"
        function main.f
        ---------------------------------------------
        def f(x: 1)
        ---------------------------------------------
        function `main.f`
        ---
        ```python
        def f(x: 1)
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.by:7:1
          |
        7 | f(1)
          | ^- Cursor offset
          | |
          | source
        ");
    }

    /// `**Name` declares a keyword-variadic pack in a basedpython file, not a
    /// `ParamSpec` — python builds a `ParamSpec` object for it at runtime, but
    /// naming the hover after that describes none of what the pack does
    #[test]
    fn hover_basedpython_keyword_pack_declaration() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                r#"
                type Alias[**Kwarg<CURSOR>s] = int
                "#,
            )
            .build();

        assert_snapshot!(test.hover(), @"
        type parameter Kwargs
        ---------------------------------------------
        KeywordPack
        ---------------------------------------------
        type parameter `Kwargs`
        ---
        ```python
        KeywordPack
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.by:2:14
          |
        2 | type Alias[**Kwargs] = int
          |              ^^^^^-
          |              |    |
          |              |    Cursor offset
          |              source
        ");
    }

    #[test]
    fn hover_dunder_file() {
        let test = hover_test(
            r#"
        __fil<CURSOR>e__
        "#,
        );

        // __file__ should be `str` when accessed within a module, not `str | None`
        assert_snapshot!(test.hover(), @"
        str
        ---------------------------------------------
        ```python
        str
        ```
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:1
          |
        2 | __file__
          | ^^^^^-^^
          | |    |
          | |    Cursor offset
          | source
        ");
    }

    // Ref: https://github.com/astral-sh/ty/issues/2401
    #[test]
    fn hover_incomplete_except_handler() {
        let test = hover_test(
            "\
try:
    print()
except <CURSOR># Trigger completion/hover here
",
        );

        assert_snapshot!(test.hover(), @"Hover provided no content");
    }

    #[test]
    fn hover_pep695_type_alias_usage() {
        let test = hover_test(
            r#"

class MyType:
    """Awesome docs"""

type U = MyType

class CoolType(str):
    u: U<CURSOR>


        "#,
        );

        assert_snapshot!(test.hover(), @"
        type alias main.U
        ---------------------------------------------
        type U = MyType
        ---------------------------------------------
        Awesome docs

        ---------------------------------------------
        type alias `main.U`
        ---
        ```python
        type U = MyType
        ```
        ---
        Awesome docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:9:8
          |
        9 |     u: U
          |        ^- Cursor offset
          |        |
          |        source
        ");
    }

    #[test]
    fn hover_pep695_type_alias_name() {
        let test = hover_test(
            r#"

class MyType:
    """Awesome docs"""

type U<CURSOR> = MyType

        "#,
        );

        assert_snapshot!(test.hover(), @"
        type alias main.U
        ---------------------------------------------
        type U = MyType
        ---------------------------------------------
        Awesome docs

        ---------------------------------------------
        type alias `main.U`
        ---
        ```python
        type U = MyType
        ```
        ---
        Awesome docs
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:6:6
          |
        6 | type U = MyType
          |      ^- Cursor offset
          |      |
          |      source
        ");
    }

    #[test]
    fn hover_type_alias_resolved_docstring_stub() {
        let test = CursorTest::builder()
            .source(
                "library.pyi",
                r#"
                class Target: ...
                type Alias = Target
                "#,
            )
            .source(
                "library.py",
                r#"
                class Target:
                    """Target implementation docs."""
                "#,
            )
            .source(
                "main.py",
                r#"
                from library import Alias
                x: Ali<CURSOR>as
                "#,
            )
            .build();

        assert_snapshot!(test.hover(), @"
        type alias library.Alias
        ---------------------------------------------
        type Alias = Target
        ---------------------------------------------
        Target implementation docs.

        ---------------------------------------------
        type alias `library.Alias`
        ---
        ```python
        type Alias = Target
        ```
        ---
        Target implementation docs.
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:3:4
          |
        3 | x: Alias
          |    ^^^-^
          |    |  |
          |    |  Cursor offset
          |    source
        ");
    }

    #[test]
    fn hover_type_alias_import_resolved_docstring_stub() {
        let test = CursorTest::builder()
            .source(
                "library.pyi",
                r#"
                class Target: ...
                type Alias = Target
                "#,
            )
            .source(
                "library.py",
                r#"
                class Target:
                    """Target implementation docs."""
                "#,
            )
            .source(
                "main.py",
                r#"
                from library import Alias<CURSOR>
                x: Alias
                "#,
            )
            .build();

        assert_snapshot!(test.hover(), @"
        type alias library.Alias
        ---------------------------------------------
        type Alias = Target
        ---------------------------------------------
        Target implementation docs.

        ---------------------------------------------
        type alias `library.Alias`
        ---
        ```python
        type Alias = Target
        ```
        ---
        Target implementation docs.
        ---------------------------------------------
        info[hover]: Hovered content is
         --> main.py:2:21
          |
        2 | from library import Alias
          |                     ^^^^^- Cursor offset
          |                     |
          |                     source
        ");
    }

    #[test]
    fn hover_type_docstring_correct_order() {
        let test = CursorTest::builder()
            .source(
                "library.py",
                r#"
                class Target:
                    """Target Docs"""

                type Alias = Target

                Copy = Alias
                """Copy Docs"""

                x: Copy<CURSOR>
                "#,
            )
            .build();

        assert_snapshot!(test.hover(), @"
        variable library.Copy
        ---------------------------------------------
        type Alias = Target
        ---------------------------------------------
        Copy Docs

        ---------------------------------------------
        Target Docs

        ---------------------------------------------
        variable `library.Copy`
        ---
        ```python
        type Alias = Target
        ```
        ---
        Copy Docs
        ---
        Target Docs
        ---------------------------------------------
        info[hover]: Hovered content is
          --> library.py:10:4
           |
        10 | x: Copy
           |    ^^^^- Cursor offset
           |    |
           |    source
        ");
    }

    impl CursorTest {
        fn hover(&self) -> String {
            use std::fmt::Write;

            let Some(hover) = hover(
                &self.db,
                self.program_file(self.cursor.file),
                self.cursor.offset,
            ) else {
                return "Hover provided no content".to_string();
            };

            let source = hover.range;

            let mut buf = String::new();

            write!(
                &mut buf,
                "{plaintext}{line}{markdown}{line}",
                plaintext = hover.display(&self.db, MarkupKind::PlainText),
                line = MarkupKind::PlainText.horizontal_line(),
                markdown = hover.display(&self.db, MarkupKind::Markdown),
            )
            .unwrap();

            let config = DisplayDiagnosticConfig::new("ty").color(false).context(0);

            let mut diagnostic = Diagnostic::new(
                DiagnosticId::Lint(LintName::of("hover")),
                Severity::Info,
                "Hovered content is",
            );
            diagnostic.annotate(
                Annotation::primary(Span::from(source.file()).with_range(source.range()))
                    .message("source"),
            );
            diagnostic.annotate(
                Annotation::secondary(
                    Span::from(source.file()).with_range(TextRange::empty(self.cursor.offset)),
                )
                .message("Cursor offset"),
            );

            write!(buf, "{}", diagnostic.display(&self.db, &config)).unwrap();

            buf
        }
    }
}
