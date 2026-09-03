use std::{fmt, vec};
use ty_python_semantic::ProgramEnvironment;

use itertools::{Either, Itertools};
use rustc_hash::FxHashMap;

use crate::importer::{ImportAction, ImportRequest, Importer, MembersInScope};
use crate::{Db, HasNavigationTargets, NavigationTarget};
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::source_order::{self, SourceOrderVisitor, TraversalSignal};
use ruff_python_ast::{
    self as ast, AnyNodeRef, ArgOrKeyword, Expr, ExprUnaryOp, PySourceType, Stmt, UnaryOp,
};
use ruff_python_codegen::Stylist;
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_module_resolver::file_to_module;
use ty_python_core::ProgramFile;
use ty_python_semantic::reified::inferred_reified_type_param_names;
use ty_python_semantic::types::context_params::implicit_context_arguments;
use ty_python_semantic::types::ide_support::{
    InlayHintCallArgumentDetails, hintable_parameter_type, implicit_enum_member_value,
    inferred_override, inferred_raises, inferred_return_annotation, inferred_type_param_variance,
    inherited_parameter_annotation, inlay_hint_call_argument_details, is_reveal_type_function,
    is_union_special_form, numeric_promotion, trailing_lambda_implicit_parameters,
    type_parameter_names,
};
use ty_python_semantic::types::{DisplaySettings, Type, TypeDetail};
use ty_python_semantic::{HasType, SemanticModel, with_display_for_file};

#[derive(Debug, Clone)]
pub struct InlayHint {
    pub position: TextSize,
    pub kind: InlayHintKind,
    pub label: InlayHintLabel,
    /// Whether the client separates the hint from the source before it.
    ///
    /// A hint that needs a space there asks for one with this rather than
    /// writing it into the label, so that the space is the client's own
    /// rendering — outside any part that links somewhere, and stylable as the
    /// gap it is rather than as label text.
    pub padding_left: bool,
    /// Whether the client separates the hint from the source after it.
    pub padding_right: bool,
    pub text_edits: Vec<InlayHintTextEdit>,
}

impl InlayHint {
    fn variable_type<'db>(
        context: InlayHintImportContext<'_, 'db>,
        expr: &Expr,
        rhs: &Expr,
        ty: Type<'db>,
        mut allow_edits: bool,
        named_type_arguments: bool,
    ) -> Option<Self> {
        let InlayHintImportContext {
            db,
            file,
            importer,
            dynamic_imports,
        } = context;

        let position = expr.range().end();
        let env = ProgramEnvironment::from_file(file);
        // Render the type to a string, and get subspans for all the types that make it up
        let settings = DisplaySettings::from_possibly_ambiguous_types(db, &env, [ty]);
        let settings = if named_type_arguments {
            settings.with_named_type_arguments()
        } else {
            settings
        };
        let details = ty.display_with(db, &env, settings).to_string_parts();

        // Filter out repetitive hints like `x: T = T()`
        if call_matches_name(rhs, &details.label) {
            return None;
        }

        let mut dynamic_importer = DynamicImporter::new(importer, expr, dynamic_imports);

        // Ok so the idea here is that we potentially have a random soup of spans here,
        // and each byte of the string can have at most one target associate with it.
        // Thankfully, they were generally pushed in print order, with the inner smaller types
        // appearing before the outer bigger ones.
        //
        // So we record where we are in the string, and every time we find a type, we
        // check if it's further along in the string. If it is, great, we give it the
        // span for its range, and then advance where we are.
        let mut offset = 0;

        // This edit label could be different from the original label if we need to
        // qualify certain imported symbols. `A` could turn into `foo.A`.
        let mut edit_label = details.label.clone();
        let mut edit_offset: isize = 0;

        let mut label_parts = vec![": ".into()];
        for (target, detail) in details.targets.iter().zip(&details.details) {
            match detail {
                TypeDetail::Type(ty) => {
                    let start = target.start().to_usize();
                    let end = target.end().to_usize();
                    // If we skipped over some bytes, push them with no target
                    if start > offset {
                        label_parts.push(details.label[offset..start].into());
                    }

                    // Possibly import the current type and return the qualified name
                    let mut qualified_name = |dynamic_importer: &mut DynamicImporter<'_, 'db>| {
                        let type_definition = ty.definition(db, &env)?;
                        let definition = type_definition.definition()?;

                        // Only module-level names can be imported with `from <module> import <name>`.
                        // If the definition lives in a class or function body we can't produce a safe edit.
                        if !definition.file_scope(db).is_global() {
                            allow_edits = false;
                            return None;
                        }

                        // Don't try to import symbols in scope
                        let definition_file = definition.file(db);
                        if definition_file == file.file(db) {
                            return None;
                        }

                        let definition_name = definition.name(db);

                        // Fallback to the label if we cannot find the name
                        let definition_name = definition_name
                            .as_deref()
                            .unwrap_or(&details.label[start..end]);

                        let file = definition.program_file(db);
                        let module = file_to_module(db, file.resolver_file(db))?;

                        if should_skip_import(db, module, *ty) {
                            return None;
                        }

                        let module_name = module.name(db).as_str();

                        dynamic_importer.import_symbol(
                            db,
                            &env,
                            ty,
                            module_name,
                            definition_name,
                            &details.label[start..end],
                        )
                    };

                    // Ok, this is the first type that claimed these bytes, give it the target
                    if start >= offset {
                        // Try to import the symbol and update the edit label if required
                        if let Some(qualified_name) = qualified_name(&mut dynamic_importer) {
                            let edit_start = (start.cast_signed() + edit_offset).cast_unsigned();
                            let edit_end = (end.cast_signed() + edit_offset).cast_unsigned();

                            edit_label.replace_range(edit_start..edit_end, &qualified_name);
                            edit_offset +=
                                qualified_name.len().cast_signed() - (end - start).cast_signed();
                        }

                        let target = ty.navigation_targets(db, &env).into_iter().next();

                        // Always use original text for the label part
                        label_parts.push(
                            InlayHintLabelPart::new(&details.label[start..end]).with_target(target),
                        );
                        offset = end;
                    }
                }
                TypeDetail::SignatureStart
                | TypeDetail::SignatureEnd
                | TypeDetail::Parameter(_) => {
                    // Don't care about these
                }
            }
        }

        // "flush" the rest of the label without any target
        if offset < details.label.len() {
            label_parts.push(details.label[offset..details.label.len()].into());
        }

        let text_edits = if details.is_valid_syntax && allow_edits {
            let mut text_edits = vec![InlayHintTextEdit {
                range: TextRange::new(position, position),
                new_text: format!(": {edit_label}"),
            }];

            text_edits.extend(dynamic_importer.text_edits());

            text_edits
        } else {
            vec![]
        };

        Some(Self {
            position,
            kind: InlayHintKind::Type,
            label: InlayHintLabel { parts: label_parts },
            padding_left: false,
            padding_right: false,
            text_edits,
        })
    }

    fn call_argument_name(
        position: TextSize,
        name: &str,
        navigation_target: Option<NavigationTarget>,
    ) -> Self {
        let label_parts = vec![
            InlayHintLabelPart::new(name).with_target(navigation_target),
            "=".into(),
        ];

        Self {
            position,
            kind: InlayHintKind::CallArgumentName,
            label: InlayHintLabel { parts: label_parts },
            padding_left: false,
            padding_right: false,
            text_edits: vec![],
        }
    }

    /// basedpython: the exception set inferred for a function with no `raises`
    /// clause, shown where the clause would be written.
    fn inferred_raises(
        db: &dyn Db,
        env: &ProgramEnvironment<'_>,
        position: TextSize,
        raised: Type,
    ) -> Self {
        Self {
            position,
            kind: InlayHintKind::Raises,
            label: InlayHintLabel {
                parts: vec![format!("raises {}", raised.display(db, env)).into()],
            },
            padding_left: true,
            padding_right: false,
            text_edits: vec![],
        }
    }

    /// basedpython: the variance inferred for a type parameter that declares
    /// none, shown where the keyword would be written.
    fn inferred_variance(position: TextSize, variance: ast::Variance) -> Self {
        let keyword = match variance {
            ast::Variance::Covariant => "out",
            ast::Variance::Contravariant => "in",
            ast::Variance::Invariant => "in out",
        };

        Self {
            position,
            kind: InlayHintKind::Variance,
            label: InlayHintLabel {
                parts: vec![keyword.into()],
            },
            padding_left: false,
            padding_right: true,
            text_edits: vec![],
        }
    }

    /// basedpython: a type parameter reified by a value-position use in the
    /// body rather than by the keyword, shown where the keyword would go.
    fn inferred_reification(position: TextSize) -> Self {
        Self {
            position,
            kind: InlayHintKind::Reification,
            label: InlayHintLabel {
                parts: vec!["reified".into()],
            },
            padding_left: false,
            padding_right: true,
            text_edits: vec![],
        }
    }

    /// basedpython: a method that overrides a superclass member without saying
    /// so, shown where the `override` modifier would be written.
    fn inferred_override(position: TextSize, superclass: Option<NavigationTarget>) -> Self {
        Self {
            position,
            kind: InlayHintKind::Override,
            label: InlayHintLabel {
                parts: vec![InlayHintLabelPart::new("override").with_target(superclass)],
            },
            padding_left: false,
            padding_right: true,
            text_edits: vec![],
        }
    }

    /// The type arguments inferred for a generic call, shown between the callee
    /// and its argument list — where an explicit specialization would be written.
    fn call_type_arguments(
        db: &dyn Db,
        env: &ProgramEnvironment<'_>,
        position: TextSize,
        arguments: &[(Name, Type)],
        named: bool,
    ) -> Self {
        let mut parts = vec!["[".into()];

        // a lone type parameter has nothing to disambiguate, so naming it would
        // be noise rather than orientation
        let named = named && arguments.len() > 1;

        for (index, (parameter, argument)) in arguments.iter().enumerate() {
            if index > 0 {
                parts.push(", ".into());
            }
            if named {
                parts.push(format!("{parameter}=").into());
            }
            parts.push(
                InlayHintLabelPart::new(argument.display(db, env).to_string())
                    .with_target(argument.navigation_targets(db, env).into_iter().next()),
            );
        }

        parts.push("]".into());

        Self {
            position,
            kind: InlayHintKind::TypeArgument,
            label: InlayHintLabel { parts },
            padding_left: false,
            padding_right: false,
            text_edits: vec![],
        }
    }

    /// basedpython: the arguments a call site fills implicitly from the
    /// `context` declarations in scope, shown where the lowering writes them.
    fn implicit_context_arguments(
        position: TextSize,
        leading_comma: bool,
        arguments: &[(&Name, &Name, Option<NavigationTarget>)],
    ) -> Self {
        let mut parts = Vec::new();

        for (index, (parameter, variable, declaration)) in arguments.iter().enumerate() {
            if leading_comma || index > 0 {
                parts.push(", ".into());
            }
            parts.push(format!("{parameter}=").into());
            parts.push(InlayHintLabelPart::new(variable.as_str()).with_target(declaration.clone()));
        }

        Self {
            position,
            kind: InlayHintKind::ImplicitArgument,
            label: InlayHintLabel { parts },
            padding_left: false,
            padding_right: false,
            text_edits: vec![],
        }
    }

    /// The name of the type parameter a positional type argument fills.
    fn type_argument_name(position: TextSize, name: &str) -> Self {
        Self {
            position,
            kind: InlayHintKind::TypeArgument,
            label: InlayHintLabel {
                parts: vec![InlayHintLabelPart::new(name), "=".into()],
            },
            padding_left: false,
            padding_right: false,
            text_edits: vec![],
        }
    }

    /// The arms the typing spec's numeric promotion adds to a `float` /
    /// `complex` type expression.
    ///
    /// `arms` is rendered without the space that separates the first `|` from
    /// the operand written before it, because that space is the client's.
    fn numeric_promotion(position: TextSize, arms: String) -> Self {
        Self {
            position,
            kind: InlayHintKind::NumericPromotion,
            label: InlayHintLabel {
                parts: vec![arms.into()],
            },
            padding_left: true,
            padding_right: false,
            text_edits: vec![],
        }
    }

    /// The value an enum member takes without the source writing one, shown
    /// after the declaration that leaves it out.
    fn enum_member_value(position: TextSize, value: &str) -> Self {
        Self {
            position,
            kind: InlayHintKind::EnumValue,
            label: InlayHintLabel {
                parts: vec![value.into()],
            },
            padding_left: true,
            padding_right: false,
            text_edits: vec![],
        }
    }

    /// The type a `reveal_type` call reveals, shown at the end of its line.
    ///
    /// `declared` is the wider type the revealed place was declared with, when the call read
    /// something narrower than the declaration allows. Showing both makes the narrowing itself
    /// visible, rather than only its result.
    fn revealed_type(
        db: &dyn Db,
        env: &ProgramEnvironment<'_>,
        position: TextSize,
        revealed: Type,
        declared: Option<Type>,
    ) -> Self {
        let label = match declared {
            Some(declared) => format!(
                "{}, narrowed from {}",
                revealed.display(db, env),
                declared.display(db, env)
            ),
            None => revealed.display(db, env).to_string(),
        };

        Self {
            position,
            kind: InlayHintKind::RevealedType,
            label: InlayHintLabel {
                parts: vec![label.into()],
            },
            padding_left: true,
            padding_right: false,
            text_edits: vec![],
        }
    }

    /// The parameters a source never spells, shown where they would be written.
    ///
    /// `leading_space` separates the hint from the character it sits after: one
    /// written inside a parameter list abuts a `(` or a `,` that already spaces
    /// it, but a trailing lambda's sits directly after the block's `:`.
    ///
    /// `parameter_follows` says a written parameter comes next, so the hint ends
    /// on the separator that keeps the list reading as source.
    fn implicit_parameters(
        db: &dyn Db,
        env: &ProgramEnvironment<'_>,
        position: TextSize,
        parameters: &[(&str, Option<Type>)],
        leading_space: bool,
        parameter_follows: bool,
    ) -> Self {
        let mut parts = Vec::new();

        for (index, (name, ty)) in parameters.iter().enumerate() {
            if index > 0 {
                parts.push(", ".into());
            }
            parts.push(InlayHintLabelPart::new(*name));
            if let Some(ty) = ty {
                parts.push(format!(": {}", ty.display(db, env)).into());
            }
        }

        if parameter_follows {
            parts.push(",".into());
        }

        Self {
            position,
            kind: InlayHintKind::ImplicitParameter,
            label: InlayHintLabel { parts },
            padding_left: leading_space,
            padding_right: parameter_follows,
            text_edits: vec![],
        }
    }

    /// basedpython: the return type recovered for a `def` that leaves its
    /// annotation out, shown where the annotation would be written.
    fn inferred_return(
        db: &dyn Db,
        env: &ProgramEnvironment<'_>,
        position: TextSize,
        returned: Type,
    ) -> Self {
        Self {
            position,
            kind: InlayHintKind::Type,
            label: InlayHintLabel {
                parts: vec![format!("-> {}", returned.display(db, env)).into()],
            },
            padding_left: true,
            padding_right: false,
            text_edits: vec![],
        }
    }

    /// The type of a parameter the source leaves unannotated, shown where the
    /// annotation would be written.
    fn parameter_type(
        db: &dyn Db,
        env: &ProgramEnvironment<'_>,
        position: TextSize,
        ty: Type,
    ) -> Self {
        Self {
            position,
            kind: InlayHintKind::Type,
            label: InlayHintLabel {
                parts: vec![format!(": {}", ty.display(db, env)).into()],
            },
            padding_left: false,
            padding_right: false,
            text_edits: vec![],
        }
    }

    pub fn display(&self) -> InlayHintDisplay<'_> {
        InlayHintDisplay { inlay_hint: self }
    }
}

#[derive(Debug, Clone)]
pub enum InlayHintKind {
    Type,
    CallArgumentName,
    /// basedpython: a function's inferred exception set
    Raises,
    /// basedpython: the variance inferred for a class type parameter
    Variance,
    /// basedpython: a type parameter reified without saying so
    Reification,
    /// The type arguments inferred for a generic call, or the name of the type
    /// parameter a positional type argument fills
    TypeArgument,
    /// basedpython: a method that overrides a superclass member without saying so
    Override,
    /// The arms the typing spec's numeric promotion adds to `float` / `complex`
    NumericPromotion,
    /// The type a `reveal_type` call reveals
    RevealedType,
    /// The value an enum member takes without the source writing one
    EnumValue,
    /// basedpython: a parameter the source never spells (`it`, `self`)
    ImplicitParameter,
    /// basedpython: an argument a call site fills from a `context` declaration
    ImplicitArgument,
}

#[derive(Debug, Clone)]
pub struct InlayHintLabel {
    parts: Vec<InlayHintLabelPart>,
}

impl InlayHintLabel {
    pub fn parts(&self) -> &[InlayHintLabelPart] {
        &self.parts
    }

    pub fn into_parts(self) -> Vec<InlayHintLabelPart> {
        self.parts
    }
}

pub struct InlayHintDisplay<'a> {
    inlay_hint: &'a InlayHint,
}

/// A hint as the client draws it, padding included, so that what a test reads is
/// what a reader of the file sees.
impl fmt::Display for InlayHintDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::fmt::Result {
        if self.inlay_hint.padding_left {
            f.write_str(" ")?;
        }
        for part in &self.inlay_hint.label.parts {
            write!(f, "{}", part.text)?;
        }
        if self.inlay_hint.padding_right {
            f.write_str(" ")?;
        }
        Ok(())
    }
}

#[derive(Default, Debug, Clone)]
pub struct InlayHintLabelPart {
    text: String,

    target: Option<NavigationTarget>,
}

impl InlayHintLabelPart {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            target: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn into_text(self) -> String {
        self.text
    }

    pub fn target(&self) -> Option<&NavigationTarget> {
        self.target.as_ref()
    }

    pub fn with_target(self, target: Option<NavigationTarget>) -> Self {
        Self { target, ..self }
    }
}

impl From<String> for InlayHintLabelPart {
    fn from(s: String) -> Self {
        Self {
            text: s,
            target: None,
        }
    }
}

impl From<&str> for InlayHintLabelPart {
    fn from(s: &str) -> Self {
        Self {
            text: s.to_string(),
            target: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct InlayHintTextEdit {
    pub range: TextRange,
    pub new_text: String,
}

pub fn inlay_hints(
    db: &dyn Db,
    file: ProgramFile<'_>,
    range: TextRange,
    settings: &InlayHintSettings,
) -> Vec<InlayHint> {
    // a hint is read as source, so it must spell types the way the file is
    // written — `1`, not `Literal[1]`, in a `.by` file
    with_display_for_file(db, file.file(db), || {
        inlay_hints_inner(db, file, range, settings)
    })
}

fn inlay_hints_inner(
    db: &dyn Db,
    file: ProgramFile<'_>,
    range: TextRange,
    settings: &InlayHintSettings,
) -> Vec<InlayHint> {
    let ast = parsed_module(db, file.python_file(db)).load(db);
    let source_file = file.file(db);

    let source = source_text(db, source_file);
    let stylist = Stylist::from_tokens(ast.tokens(), source.as_str());
    let importer = Importer::new(db, &stylist, file, source.as_str(), &ast);

    let mut visitor = InlayHintVisitor::new(db, file, importer, source.as_str(), range, settings);

    visitor.visit_body(ast.suite());

    // hints are collected in visit order, which is not quite source order: a
    // `raises` or `override` hint is added before the definition it sits on is
    // walked into, and a revealed type lands at the end of its own line
    visitor.hints.sort_by_key(|hint| hint.position);

    visitor.hints
}

/// Settings to control the behavior of inlay hints.
#[derive(Clone, Debug)]
#[expect(clippy::struct_excessive_bools, reason = "one toggle per hint kind")]
pub struct InlayHintSettings {
    /// Whether to show variable type hints.
    ///
    /// For example, this would enable / disable hints like the ones quoted below:
    /// ```python
    /// x": Literal[1]" = 1
    /// ```
    pub variable_types: bool,

    /// Whether to show call argument names.
    ///
    /// For example, this would enable / disable hints like the ones quoted below:
    /// ```python
    /// def foo(x: int): pass
    /// foo("x="1)
    /// ```
    pub call_argument_names: bool,

    /// basedpython: whether to show the exception set inferred for a function
    /// that has no `raises` clause.
    ///
    /// For example, this would enable / disable hints like the one quoted below:
    /// ```by
    /// def f()" raises TypeError":
    ///     raise TypeError
    /// ```
    pub inferred_raises: bool,

    /// basedpython: whether to show the variance ty infers for a class type
    /// parameter that does not declare one.
    ///
    /// ```by
    /// class A["out "T]:
    ///     def get(self) -> T: ...
    /// ```
    pub inferred_variance: bool,

    /// basedpython: whether to show `reified` on a function type parameter that
    /// the body reifies without saying so.
    ///
    /// ```by
    /// def f["reified "T]():
    ///     print(T)
    /// ```
    pub inferred_reification: bool,

    /// Whether to show the type arguments inferred for a generic call.
    ///
    /// ```by
    /// def identity[T](x: T) -> T: ...
    /// identity"[int]"(1)
    /// ```
    pub call_type_arguments: bool,

    /// Whether to show the name of the type parameter a positional type
    /// argument fills.
    ///
    /// ```python
    /// x: dict["_KT="str, "_VT="int]
    /// ```
    pub type_argument_names: bool,

    /// basedpython: whether to show `override` on a method that overrides a
    /// superclass member without saying so.
    ///
    /// ```by
    /// class B(A):
    ///     "override "def f(self): ...
    /// ```
    pub inferred_override: bool,

    /// Whether to show the extra arms the typing spec's numeric promotion adds
    /// to `float` and `complex` in a type expression.
    ///
    /// ```python
    /// def f(x: float" | int"): ...
    /// ```
    pub numeric_promotions: bool,

    /// Whether to show the type a `reveal_type` call reveals, at the end of its
    /// line. A place declared wider than what the call read shows both types.
    ///
    /// ```python
    /// reveal_type(1)" Literal[1]"
    /// ```
    pub revealed_types: bool,

    /// basedpython: whether to show the parameters a trailing lambda binds but
    /// the source never spells — `it`, and the receiver spelled `self`.
    ///
    /// ```by
    /// f(2)"it: int":
    ///     print(it)
    /// ```
    pub implicit_parameters: bool,

    /// basedpython: whether to show the `self` an `init(...)` binds without
    /// spelling it.
    ///
    /// ```by
    /// class C:
    ///     init("self, "a: int)
    /// ```
    pub implicit_self: bool,

    /// Whether to show the inferred type of an unannotated lambda parameter.
    ///
    /// ```python
    /// map(lambda x": int": x + 1, [1])
    /// ```
    pub lambda_parameter_types: bool,

    /// basedpython: whether to show the type an unannotated parameter takes from
    /// the method it overrides or the overloads it implements.
    ///
    /// ```by
    /// class B(A):
    ///     def f(self, a": int"): ...
    /// ```
    pub inherited_parameter_types: bool,

    /// basedpython: whether to show the return type recovered for a `def` that
    /// leaves its annotation out.
    ///
    /// ```by
    /// def f()" -> 1":
    ///     return 1
    /// ```
    pub inferred_return_types: bool,

    /// basedpython: whether to show the arguments a call site fills implicitly
    /// from the `context` declarations in scope.
    ///
    /// ```by
    /// def f(context a: int): ...
    ///
    /// context b = 1
    /// f("a=b")
    /// ```
    pub implicit_arguments: bool,

    /// Whether to show the value an enum member takes without the source
    /// writing one.
    ///
    /// ```by
    /// enum class Color:
    ///     case Red" 1", Green" 2"
    /// ```
    pub enum_values: bool,

    /// django templates: whether to show the element type a `{% for %}` binding
    /// takes.
    ///
    /// ```django-html
    /// {% for book": Book" in shelf %}
    /// ```
    pub template_binding_types: bool,

    /// django templates: whether to show the file an `{% extends %}` or an
    /// `{% include %}` name resolves to.
    ///
    /// ```django-html
    /// {% include "card.html"" → blog/templates/blog/card.html" %}
    /// ```
    pub resolved_templates: bool,
    // Add any new setting that enables additional inlays to `any_enabled`.
}

impl InlayHintSettings {
    /// Every hint disabled — a base for enabling one kind at a time.
    pub fn none() -> Self {
        Self {
            variable_types: false,
            call_argument_names: false,
            inferred_raises: false,
            inferred_variance: false,
            inferred_reification: false,
            call_type_arguments: false,
            type_argument_names: false,
            inferred_override: false,
            numeric_promotions: false,
            revealed_types: false,
            implicit_parameters: false,
            implicit_self: false,
            lambda_parameter_types: false,
            inherited_parameter_types: false,
            inferred_return_types: false,
            implicit_arguments: false,
            enum_values: false,
            template_binding_types: false,
            resolved_templates: false,
        }
    }

    pub fn any_enabled(&self) -> bool {
        let Self {
            variable_types,
            call_argument_names,
            inferred_raises,
            inferred_variance,
            inferred_reification,
            call_type_arguments,
            type_argument_names,
            inferred_override,
            numeric_promotions,
            revealed_types,
            implicit_parameters,
            implicit_self,
            lambda_parameter_types,
            inherited_parameter_types,
            inferred_return_types,
            implicit_arguments,
            enum_values,
            template_binding_types,
            resolved_templates,
        } = *self;

        variable_types
            || call_argument_names
            || inferred_raises
            || inferred_variance
            || inferred_reification
            || call_type_arguments
            || type_argument_names
            || inferred_override
            || numeric_promotions
            || revealed_types
            || implicit_parameters
            || implicit_self
            || lambda_parameter_types
            || inherited_parameter_types
            || inferred_return_types
            || implicit_arguments
            || enum_values
            || template_binding_types
            || resolved_templates
    }
}

impl Default for InlayHintSettings {
    fn default() -> Self {
        Self {
            variable_types: true,
            call_argument_names: true,
            inferred_raises: true,
            inferred_variance: true,
            inferred_reification: true,
            call_type_arguments: true,
            type_argument_names: true,
            inferred_override: true,
            numeric_promotions: true,
            revealed_types: true,
            implicit_parameters: true,
            implicit_self: true,
            lambda_parameter_types: true,
            inherited_parameter_types: true,
            inferred_return_types: true,
            implicit_arguments: true,
            enum_values: true,
            template_binding_types: true,
            resolved_templates: true,
        }
    }
}

struct InlayHintImportContext<'a, 'db> {
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    importer: &'a Importer<'db>,
    dynamic_imports: &'a mut FxHashMap<DynamicallyImportedMember, ImportAction>,
}

struct InlayHintVisitor<'a, 'db> {
    db: &'db dyn Db,
    model: SemanticModel<'db>,
    /// Imports that we have already created.
    /// We store these imports so that we don't create multiple imports for the same symbol.
    dynamic_imports: FxHashMap<DynamicallyImportedMember, ImportAction>,
    importer: Importer<'db>,
    source: &'a str,
    hints: Vec<InlayHint>,
    assignment_rhs: Option<&'a Expr>,
    range: TextRange,
    settings: &'a InlayHintSettings,
    in_no_edits_allowed: bool,
    /// The kind of file being hinted. Several hints only exist in basedpython,
    /// and reification reads it directly: `**P` is a reifiable keyword pack
    /// only in a basedpython source file.
    source_type: PySourceType,
    /// Whether we are inside a type expression, where `float` promotes and a
    /// subscript's arguments fill type parameters.
    in_type_expression: bool,
    /// Whether we are inside a `lambda`'s parameter list.
    in_lambda: bool,
    /// The operand of a union in a type expression currently being visited, and what the
    /// operands written beside it denote. Read by numeric promotion, which adds nothing an
    /// operand already spells.
    union_siblings: Option<(TextRange, Vec<Type<'db>>)>,
    /// The class whose body we are directly inside, if any.
    enclosing_class: Option<Type<'db>>,
}

impl<'a, 'db> InlayHintVisitor<'a, 'db> {
    fn new(
        db: &'db dyn Db,
        file: ProgramFile<'db>,
        importer: Importer<'db>,
        source: &'a str,
        range: TextRange,
        settings: &'a InlayHintSettings,
    ) -> Self {
        Self {
            db,
            model: SemanticModel::new(db, file),
            dynamic_imports: FxHashMap::default(),
            importer,
            source,
            hints: Vec::new(),
            assignment_rhs: None,
            range,
            settings,
            in_no_edits_allowed: false,
            source_type: file.file(db).source_type(db),
            in_type_expression: false,
            in_lambda: false,
            union_siblings: None,
            enclosing_class: None,
        }
    }

    /// Whether the file uses basedpython syntax, which several hints spell.
    fn is_basedpython(&self) -> bool {
        self.source_type.is_basedpython()
    }

    fn add_type_hint(&mut self, expr: &Expr, rhs: &Expr, ty: Type<'db>, allow_edits: bool) {
        if !self.settings.variable_types {
            return;
        }

        // the hint is an annotation the user can accept, so it offers the type the
        // declaration would have. basedpython infers `A()` as `final A`, but the
        // declaration it widens to is `A` — writing the modifier down would be a
        // stricter declaration than the code asked for
        let ty = ty.erase_restriction(self.db);

        if is_ignored_variable_assignment_target(expr) {
            return;
        }

        let named_type_arguments = self.names_type_arguments();

        let context = InlayHintImportContext {
            db: self.db,
            file: self.model.program_file(),
            importer: &self.importer,
            dynamic_imports: &mut self.dynamic_imports,
        };

        if let Some(inlay_hint) =
            InlayHint::variable_type(context, expr, rhs, ty, allow_edits, named_type_arguments)
        {
            self.hints.push(inlay_hint);
        }
    }

    /// basedpython: hint the exception set of a function with no `raises` clause.
    ///
    /// The hint sits where the clause would be written — after the return
    /// annotation, before the `:` — so accepting it reads as ordinary source.
    fn add_inferred_raises(&mut self, function: &ast::StmtFunctionDef) {
        let env = &self.model.program_environment();
        if !self.settings.inferred_raises || function.raises.is_some() {
            return;
        }

        let Some(raised) = function
            .inferred_type(&self.model)
            .and_then(|ty| inferred_raises(self.db, env, ty))
        else {
            return;
        };

        let position = function
            .returns
            .as_deref()
            .map_or_else(|| function.parameters.end(), Ranged::end);

        self.hints
            .push(InlayHint::inferred_raises(self.db, env, position, raised));
    }

    /// basedpython: hint the variance ty infers for each type parameter of
    /// `class` that does not declare one.
    fn add_inferred_variances(
        &mut self,
        type_params: Option<&ast::TypeParams>,
        owner: impl FnOnce(&SemanticModel<'db>) -> Option<Type<'db>>,
    ) {
        if !self.settings.inferred_variance || !self.is_basedpython() {
            return;
        }

        let Some(type_params) = type_params else {
            return;
        };

        // only look the owner up once, and only when something could be hinted.
        // a variadic or keyword-variadic pack has no variance syntax of its own,
        // so its variance is always inferred and always worth showing
        let undeclared = || {
            type_params.iter().filter(|type_param| match type_param {
                ast::TypeParam::TypeVar(type_var) => type_var.variance.is_none(),
                ast::TypeParam::TypeVarTuple(_) | ast::TypeParam::ParamSpec(_) => true,
            })
        };

        if undeclared().next().is_none() {
            return;
        }

        let Some(owner) = owner(&self.model) else {
            return;
        };

        for type_param in undeclared() {
            let Some(variance) =
                inferred_type_param_variance(self.db, owner, type_param.name().as_str())
            else {
                continue;
            };

            self.hints.push(InlayHint::inferred_variance(
                type_param.range().start(),
                variance,
            ));
        }
    }

    /// basedpython: hint `reified` on each type parameter of `function` that a
    /// value-position use in the body reifies without saying so.
    fn add_inferred_reification(&mut self, function: &ast::StmtFunctionDef) {
        if !self.settings.inferred_reification || !self.is_basedpython() {
            return;
        }

        let Some(type_params) = function.type_params.as_deref() else {
            return;
        };

        let inferred = inferred_reified_type_param_names(self.source, self.source_type, function);

        // the hint goes where the keyword would be written, which is ahead of
        // everything the parameter's own declaration spells
        for type_param in type_params {
            if inferred.contains(&type_param.name().id) {
                self.hints
                    .push(InlayHint::inferred_reification(type_param.range().start()));
            }
        }
    }

    /// Hint the value an enum member takes when its declaration does not write
    /// one, at `position` — after `auto()`, or after a `case` variant's name.
    fn add_enum_member_value(&mut self, name: &str, position: TextSize) {
        if !self.settings.enum_values {
            return;
        }

        let Some(class_ty) = self.enclosing_class else {
            return;
        };

        let env = &self.model.program_environment();
        let Some(value) = implicit_enum_member_value(self.db, env, class_ty, name) else {
            return;
        };

        // a value ty only knows the type of — an enum with a mixin whose
        // `auto()` it cannot follow — says nothing a reader wants written here
        let Some(rendered) = value.display_value(self.db, env) else {
            return;
        };

        self.hints.push(InlayHint::enum_member_value(
            position,
            &rendered.to_string(),
        ));
    }

    /// basedpython: hint `override` on a method that overrides a superclass
    /// member without saying so.
    fn add_inferred_override(&mut self, function: &ast::StmtFunctionDef) {
        let env = &self.model.program_environment();
        if !self.settings.inferred_override || !self.is_basedpython() {
            return;
        }

        let Some(class_ty) = self.enclosing_class else {
            return;
        };

        let Some(superclass) = function
            .inferred_type(&self.model)
            .and_then(|ty| inferred_override(self.db, env, class_ty, ty, &function.name))
        else {
            return;
        };

        // the range of a `def` excludes its decorators, so this is the modifier
        // position even on a decorated method
        self.hints.push(InlayHint::inferred_override(
            function.range().start(),
            superclass
                .navigation_targets(self.db, env)
                .into_iter()
                .next(),
        ));
    }

    /// Hint the type arguments inferred for a generic call.
    fn add_call_type_arguments(&mut self, call: &ast::ExprCall, arguments: &[(Name, Type<'db>)]) {
        let env = &self.model.program_environment();
        if !self.settings.call_type_arguments || arguments.is_empty() {
            return;
        }

        // an explicit specialization is already written out, and a call whose
        // type arguments all went unsolved says nothing worth reading
        if call.func.is_subscript_expr()
            || arguments.iter().all(|(_, argument)| argument.is_unknown())
        {
            return;
        }

        self.hints.push(InlayHint::call_type_arguments(
            self.db,
            env,
            call.func.range().end(),
            arguments,
            self.names_type_arguments(),
        ));
    }

    /// Whether a rendered type argument should name the parameter it fills.
    ///
    /// Only in a `.by` file: `A[Key=str]` is basedpython's keyword subscript,
    /// and a hint is read as source — python's subscript grammar has no keyword
    /// form, so naming there would spell something unwritable.
    fn names_type_arguments(&self) -> bool {
        self.settings.type_argument_names && self.is_basedpython()
    }

    /// basedpython: hint the arguments a call site fills implicitly from the
    /// `context` declarations in scope, written where the lowering writes them
    /// — after the explicit arguments, by keyword.
    fn add_implicit_context_arguments(&mut self, call: &ast::ExprCall, callee: Option<Type<'db>>) {
        let env = &self.model.program_environment();
        if !self.settings.implicit_arguments || !self.is_basedpython() {
            return;
        }

        let Some(callee) = callee else {
            return;
        };

        let arguments = implicit_context_arguments(self.db, env, self.model.file(), callee, call);
        if arguments.is_empty() {
            return;
        }

        let labelled: Vec<_> = arguments
            .iter()
            .map(|argument| {
                (
                    &argument.parameter,
                    &argument.variable,
                    argument.declaration.map(NavigationTarget::from),
                )
            })
            .collect();

        // the lowering appends to the explicit arguments, so an empty call
        // takes the position just inside its `(`
        let last_explicit = call.arguments.iter_source_order().last();
        let position = last_explicit.map_or_else(
            || call.arguments.range().start() + TextSize::from(1),
            |argument| argument.range().end(),
        );

        self.hints.push(InlayHint::implicit_context_arguments(
            position,
            last_explicit.is_some(),
            &labelled,
        ));
    }

    /// Hint the name of the type parameter each positional argument of a
    /// subscripted generic fills.
    fn add_type_argument_names(&mut self, subscript: &ast::ExprSubscript) {
        if !self.settings.type_argument_names {
            return;
        }

        let Some(names) = subscript
            .value
            .inferred_type(&self.model)
            .and_then(|ty| type_parameter_names(self.db, ty))
        else {
            return;
        };

        // a single type parameter has nothing to disambiguate, and a count
        // mismatch means a variadic generic with no fixed parameter per argument
        if names.len() < 2 || names.len() != subscript_arguments(&subscript.slice).count() {
            return;
        }

        for (name, argument) in names.iter().zip(subscript_arguments(&subscript.slice)) {
            self.hints.push(InlayHint::type_argument_name(
                argument.range().start(),
                name,
            ));
        }
    }

    /// Hint the extra arms the typing spec's numeric promotion adds to a
    /// `float` / `complex` type expression.
    fn add_numeric_promotion(&mut self, expr: &Expr) {
        let env = &self.model.program_environment();
        if !self.settings.numeric_promotions || !self.in_type_expression {
            return;
        }

        let arms = {
            // the siblings belong to one operand, so anything nested inside that operand — the
            // `float` of `list[float] | int` — sits in a union of its own and has none
            let siblings = match &self.union_siblings {
                Some((operand, siblings)) if *operand == expr.range() => siblings.as_slice(),
                _ => &[][..],
            };

            expr.inferred_type(&self.model)
                .and_then(|ty| numeric_promotion(self.db, env, self.model.file(), ty, siblings))
        };

        let Some(arms) = arms else {
            return;
        };

        self.hints
            .push(InlayHint::numeric_promotion(expr.range().end(), arms));
    }

    /// Visit the operands of a union written in a type expression, each knowing what the
    /// operands written beside it already denote.
    ///
    /// A union is the one place a promoted arm can already be spelled: `float | int` names two
    /// arms whichever way it is read, so the promotion adds nothing there.
    fn visit_union_operands(&mut self, operands: &[&'a Expr]) {
        let types: Vec<_> = operands
            .iter()
            .map(|operand| operand.inferred_type(&self.model))
            .collect();

        let outer = self.union_siblings.take();

        for (index, operand) in operands.iter().enumerate() {
            let siblings = types
                .iter()
                .enumerate()
                .filter(|(sibling, _)| *sibling != index)
                .filter_map(|(_, ty)| *ty)
                .collect();

            self.union_siblings = Some((operand.range(), siblings));
            self.visit_expr(operand);
        }

        self.union_siblings = outer;
    }

    /// Hint the type a `reveal_type` call reveals, at the end of its line.
    fn add_revealed_type(&mut self, call: &ast::ExprCall) {
        let env = &self.model.program_environment();
        if !self.settings.revealed_types {
            return;
        }

        let Some(argument) = call.arguments.args.iter().exactly_one().ok() else {
            return;
        };
        let Some(revealed) = argument.inferred_type(&self.model) else {
            return;
        };

        self.hints.push(InlayHint::revealed_type(
            self.db,
            env,
            self.source.line_end(call.range().end()),
            revealed,
            self.model
                .declared_type_at_load(ast::ExprRef::from(argument), revealed),
        ));
    }

    /// basedpython: hint the `self` an `init(...)` binds without spelling it.
    /// The parser gives such a parameter an empty range at the position it
    /// would occupy.
    ///
    /// A property accessor binds one too, but its whole header is synthesized
    /// and skipped before this is reached — the construct is hinted at its
    /// head instead.
    fn add_implicit_self(&mut self, parameter: &ast::Parameter) {
        let env = &self.model.program_environment();
        if !self.settings.implicit_self || !self.is_basedpython() || !parameter.range().is_empty() {
            return;
        }

        let position = parameter.range().start();
        let ty = hintable_parameter_type(&self.model, parameter);

        self.hints.push(InlayHint::implicit_parameters(
            self.db,
            env,
            position,
            &[(parameter.name.as_str(), ty)],
            false,
            self.parameter_follows(position),
        ));
    }

    /// basedpython: hint the parameters a trailing lambda block binds — `it`,
    /// preceded by the receiver spelled `self` when the callback declares one.
    ///
    /// The parser anchors the synthetic parameter *on* the block's `:`, but the
    /// binding belongs to the suite that opens after it, so the hint sits past
    /// the colon rather than between the callee and it.
    fn add_trailing_lambda_parameter(&mut self, function: &ast::StmtFunctionDef) {
        let env = &self.model.program_environment();
        if !self.settings.implicit_parameters {
            return;
        }

        let Some(parameter) = function.parameters.args.first() else {
            return;
        };

        let colon = parameter.range().start();
        if self.source.as_bytes().get(colon.to_usize()) != Some(&b':') {
            return;
        }

        let parameters = trailing_lambda_implicit_parameters(&self.model, function);
        if parameters.is_empty() {
            return;
        }

        self.hints.push(InlayHint::implicit_parameters(
            self.db,
            env,
            colon + TextSize::from(1),
            &parameters,
            true,
            false,
        ));
    }

    /// Whether a parameter of the same list is written after `position`, so an
    /// implicit one placed there needs a separator to read as source.
    fn parameter_follows(&self, position: TextSize) -> bool {
        self.source
            .get(position.to_usize()..)
            .is_some_and(|rest| !rest.trim_start().starts_with([')', ',']))
    }

    /// Hint the inferred type of an unannotated lambda parameter.
    fn add_lambda_parameter_type(&mut self, parameter: &ast::Parameter) {
        let env = &self.model.program_environment();
        if !self.settings.lambda_parameter_types
            || !self.in_lambda
            || parameter.annotation.is_some()
            || parameter.range().is_empty()
        {
            return;
        }

        let Some(ty) = hintable_parameter_type(&self.model, parameter) else {
            return;
        };

        self.hints.push(InlayHint::parameter_type(
            self.db,
            env,
            parameter.name.range().end(),
            ty,
        ));
    }

    /// basedpython: hint the type an unannotated parameter of a `def` takes from
    /// the method it overrides or the overloads it implements.
    ///
    /// A lambda's parameters are typed from the call site instead, and are hinted
    /// under their own setting.
    fn add_inherited_parameter_type(&mut self, parameter: &ast::Parameter) {
        let env = &self.model.program_environment();
        if !self.settings.inherited_parameter_types
            || self.in_lambda
            || parameter.annotation.is_some()
            || parameter.range().is_empty()
        {
            return;
        }

        let Some(ty) = inherited_parameter_annotation(&self.model, parameter) else {
            return;
        };

        self.hints.push(InlayHint::parameter_type(
            self.db,
            env,
            parameter.name.range().end(),
            ty,
        ));
    }

    /// basedpython: hint the return type recovered for a `def` that leaves its
    /// annotation out, where the annotation would be written.
    ///
    /// This runs before the `raises` hint, which sits in the same place when
    /// there is no annotation, so the two read in the order they are written:
    /// `def f() -> int raises TypeError`.
    fn add_inferred_return(&mut self, function: &ast::StmtFunctionDef) {
        let env = &self.model.program_environment();
        if !self.settings.inferred_return_types
            || function.returns.is_some()
            || function.is_asserts_return
        {
            return;
        }

        let Some(returned) = function
            .inferred_type(&self.model)
            .and_then(|ty| inferred_return_annotation(self.db, ty))
        else {
            return;
        };

        self.hints.push(InlayHint::inferred_return(
            self.db,
            env,
            function.parameters.end(),
            returned,
        ));
    }

    /// Visit an expression that denotes a type rather than a value.
    fn visit_type_expr(&mut self, expr: &'a Expr) {
        let in_type_expression = std::mem::replace(&mut self.in_type_expression, true);
        self.visit_expr(expr);
        self.in_type_expression = in_type_expression;
    }

    fn add_call_argument_name(
        &mut self,
        position: TextSize,
        name: &str,
        navigation_target: Option<NavigationTarget>,
    ) -> bool {
        if !self.settings.call_argument_names {
            return false;
        }

        if name.starts_with('_') {
            return false;
        }

        let inlay_hint = InlayHint::call_argument_name(position, name, navigation_target);

        self.hints.push(inlay_hint);
        true
    }
}

impl<'a> SourceOrderVisitor<'a> for InlayHintVisitor<'a, '_> {
    fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
        if self.range.intersect(node.range()).is_some() {
            TraversalSignal::Traverse
        } else {
            TraversalSignal::Skip
        }
    }

    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        let node = AnyNodeRef::from(stmt);

        if !self.enter_node(node).is_traverse() {
            return;
        }

        match stmt {
            Stmt::Assign(assign) => {
                // basedpython: a decorator may be written above a binding. A
                // decorated `def` reaches its decorators through the walk below;
                // this arm returns before that, so they are visited here
                for decorator in &assign.decorator_list {
                    self.visit_decorator(decorator);
                }

                // the value goes where `auto()` stands in for it
                if let [Expr::Name(target)] = assign.targets.as_slice() {
                    self.add_enum_member_value(&target.id, assign.value.range().end());
                }

                if !type_hint_is_excessive_for_expr(&assign.value) {
                    self.assignment_rhs = Some(&*assign.value);
                }
                if !annotations_are_valid_syntax(assign) {
                    self.in_no_edits_allowed = true;
                }
                for target in &assign.targets {
                    self.visit_expr(target);
                }
                self.in_no_edits_allowed = false;
                self.assignment_rhs = None;

                self.visit_expr(&assign.value);

                return;
            }
            // basedpython: a declaration that names no type is hinted like the
            // assignment it is — the type goes where the declaration would
            // write it, after the name
            Stmt::AnnAssign(assign) if let Some(value) = untyped_declaration_value(assign) => {
                // as on a plain assignment, a decorator above the declaration is
                // real source and is visited here rather than by the walk below
                for decorator in &assign.decorator_list {
                    self.visit_decorator(decorator);
                }

                if !type_hint_is_excessive_for_expr(value) {
                    self.assignment_rhs = Some(value);
                }
                self.visit_expr(&assign.target);
                self.assignment_rhs = None;

                self.visit_expr(value);

                return;
            }
            Stmt::Expr(expr) => {
                self.visit_expr(&expr.value);
                return;
            }
            Stmt::FunctionDef(function) if function.is_trailing_lambda => {
                self.add_trailing_lambda_parameter(function);

                // the whole header is synthetic — the callee rides on a
                // decorator and the `it` parameter has no source of its own —
                // so walk the two parts that are real
                for decorator in &function.decorator_list {
                    self.visit_decorator(decorator);
                }

                let enclosing_class = self.enclosing_class.take();
                self.visit_body(&function.body);
                self.enclosing_class = enclosing_class;

                return;
            }
            // basedpython: a property accessor's whole header is synthesized — the
            // parameter list stands for no source and the name and declared type
            // belong to the construct's head, which is written once and hinted
            // there — so only the accessor body is real
            Stmt::FunctionDef(function) if has_synthesized_header(function) => {
                let enclosing_class = self.enclosing_class.take();
                self.visit_body(&function.body);
                self.enclosing_class = enclosing_class;

                return;
            }
            Stmt::FunctionDef(function) => {
                self.add_inferred_return(function);
                self.add_inferred_raises(function);
                self.add_inferred_reification(function);
                self.add_inferred_override(function);

                // a function nested in a method is not itself a class member
                let enclosing_class = self.enclosing_class.take();
                source_order::walk_stmt(self, stmt);
                self.enclosing_class = enclosing_class;

                return;
            }
            Stmt::ClassDef(class) => {
                self.add_inferred_variances(class.type_params.as_deref(), |model| {
                    class.inferred_type(model)
                });

                // a `case` variant is a member declaration with nowhere to write
                // a value, so it goes after the name — `case Red 1, Green 2`
                if class.is_enum_variant() {
                    self.add_enum_member_value(&class.name.id, class.range().end());
                }

                let enclosing_class =
                    std::mem::replace(&mut self.enclosing_class, class.inferred_type(&self.model));
                source_order::walk_stmt(self, stmt);
                self.enclosing_class = enclosing_class;

                return;
            }
            Stmt::TypeAlias(type_alias) => {
                self.add_inferred_variances(type_alias.type_params.as_deref(), |model| {
                    type_alias.inferred_type(model)
                });

                self.visit_expr(&type_alias.name);

                if let Some(type_params) = &type_alias.type_params {
                    self.visit_type_params(type_params);
                }

                self.visit_type_expr(&type_alias.value);

                return;
            }
            Stmt::For(_) => {}
            _ => {}
        }

        source_order::walk_stmt(self, stmt);
    }

    fn visit_annotation(&mut self, expr: &'a Expr) {
        self.visit_type_expr(expr);
    }

    fn visit_parameter(&mut self, parameter: &'a ast::Parameter) {
        if self.enter_node(parameter.into()).is_traverse() {
            self.add_implicit_self(parameter);
            self.add_lambda_parameter_type(parameter);
            self.add_inherited_parameter_type(parameter);
        }

        source_order::walk_parameter(self, parameter);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Name(name) => {
                if let Some(rhs) = self.assignment_rhs {
                    if name.ctx.is_store() {
                        if let Some(ty) = expr.inferred_type(&self.model) {
                            self.add_type_hint(expr, rhs, ty, !self.in_no_edits_allowed);
                        }
                    }
                }
                self.add_numeric_promotion(expr);
                source_order::walk_expr(self, expr);
            }
            Expr::Attribute(attribute) => {
                if let Some(rhs) = self.assignment_rhs {
                    if attribute.ctx.is_store() {
                        if let Some(ty) = expr.inferred_type(&self.model) {
                            self.add_type_hint(expr, rhs, ty, !self.in_no_edits_allowed);
                        }
                    }
                }
                self.add_numeric_promotion(expr);
                source_order::walk_expr(self, expr);
            }
            Expr::Lambda(_) => {
                // every parameter below a lambda belongs to a lambda
                let in_lambda = std::mem::replace(&mut self.in_lambda, true);
                source_order::walk_expr(self, expr);
                self.in_lambda = in_lambda;
            }
            // a union in a type expression, written either way. its operands are visited by
            // hand so each one knows what the others already spell
            Expr::BinOp(binop)
                if self.in_type_expression && matches!(binop.op, ast::Operator::BitOr) =>
            {
                if !self.enter_node(expr.into()).is_traverse() {
                    return;
                }

                let mut operands = Vec::new();
                flatten_bit_or(expr, &mut operands);
                self.visit_union_operands(&operands);
            }
            Expr::Subscript(subscript)
                if self.in_type_expression
                    && subscript
                        .value
                        .inferred_type(&self.model)
                        .is_some_and(is_union_special_form) =>
            {
                if !self.enter_node(expr.into()).is_traverse() {
                    return;
                }

                self.add_type_argument_names(subscript);
                self.visit_expr(&subscript.value);

                let operands: Vec<_> = subscript_arguments(&subscript.slice).collect();
                self.visit_union_operands(&operands);
            }
            Expr::Subscript(subscript) if self.in_type_expression => {
                self.add_type_argument_names(subscript);
                source_order::walk_expr(self, expr);
            }
            Expr::Call(call) => {
                let callee = call.func.inferred_type(&self.model);
                let reveals_type =
                    callee.is_some_and(|callee| is_reveal_type_function(self.db, callee));

                if reveals_type {
                    self.add_revealed_type(call);
                }

                // a string tag's argument is the abutting literal, not something
                // the reader passed by position, and a `cast` operator's are its
                // own surface syntax
                let details = if call.is_string_tag || call.cast_kind.is_some() {
                    InlayHintCallArgumentDetails::default()
                } else {
                    inlay_hint_call_argument_details(self.db, &self.model, call).unwrap_or_default()
                };

                // `reveal_type`'s own type argument *is* the revealed type,
                // which the hint above already spells out
                if !reveals_type {
                    self.add_call_type_arguments(call, &details.type_arguments);
                }
                self.add_implicit_context_arguments(call, callee);

                self.visit_expr(&call.func);

                let mut last_editable_hint_index: Option<usize> = None;

                // `argument_names` is keyed by positional-arg index, not source-order index,
                // so track them separately to stay in sync after keyword args appear mid-call.
                let mut positional_index = 0;
                for arg_or_keyword in call.arguments.iter_source_order() {
                    if let ArgOrKeyword::Arg(argument) = arg_or_keyword {
                        if let Some((name, parameter_label_offset)) =
                            details.argument_names.get(&positional_index)
                            && !arg_matches_name(argument, name)
                        {
                            if self.add_call_argument_name(
                                arg_or_keyword.range().start(),
                                name,
                                parameter_label_offset.map(NavigationTarget::from),
                            ) {
                                if !argument.is_starred_expr() {
                                    last_editable_hint_index = Some(self.hints.len() - 1);
                                }
                            }
                        }

                        positional_index += 1;
                    }

                    self.visit_expr(arg_or_keyword.value());
                }

                // For the last positional argument, provide an edit to insert
                // the inlay hint.
                if let Some(index) = last_editable_hint_index {
                    let hint: &mut InlayHint = &mut self.hints[index];
                    hint.text_edits = vec![InlayHintTextEdit {
                        range: TextRange::empty(hint.position),
                        new_text: format!("{}=", hint.label.parts()[0].text()),
                    }];
                }
            }
            _ => {
                source_order::walk_expr(self, expr);
            }
        }
    }
}

/// The operands a `|` union writes, flattened across the whole chain: `a | b | c` names three
/// arms, not a union holding a union.
fn flatten_bit_or<'a>(expr: &'a Expr, operands: &mut Vec<&'a Expr>) {
    if let Expr::BinOp(binop) = expr
        && matches!(binop.op, ast::Operator::BitOr)
    {
        flatten_bit_or(&binop.left, operands);
        flatten_bit_or(&binop.right, operands);
    } else {
        operands.push(expr);
    }
}

/// The arguments a subscript passes, one per element of its slice.
///
/// A basedpython keyword subscript (`M[V=int]`) already names the parameter it
/// fills, so a slice carrying one has nothing to hint and yields no arguments.
fn subscript_arguments(slice: &Expr) -> impl Iterator<Item = &Expr> {
    let elements = match slice {
        Expr::Tuple(tuple) if !tuple.parenthesized => Either::Left(tuple.elts.iter()),
        _ => Either::Right(std::iter::once(slice)),
    };

    // a keyword element's target is parser-synthesized, which `Invalid` marks
    let has_keyword = elements.clone().any(|element| {
        element
            .as_named_expr()
            .and_then(|named| named.target.as_name_expr())
            .is_some_and(|target| target.ctx.is_invalid())
    });

    elements.filter(move |_| !has_keyword)
}

/// Given a positional argument, check if the expression is the "same name"
/// as the function argument itself.
///
/// This allows us to filter out repetitive inlay hints like `x=x`, `x=y.x`, etc.,
/// and suppresses hints for arguments that are already explicit keyword arguments.
fn arg_matches_name(argument: &Expr, name: &str) -> bool {
    let mut expr = argument;
    loop {
        match expr {
            // `x=x(1, 2)` counts as a match, recurse for it
            Expr::Call(expr_call) => expr = &expr_call.func,
            // `x=x[0]` is a match, recurse for it
            Expr::Subscript(expr_subscript) => expr = &expr_subscript.value,
            // `x=x` is a match
            Expr::Name(expr_name) => return name_matches_parameter(expr_name.id.as_str(), name),
            // `x=y.x` is a match
            Expr::Attribute(expr_attribute) => {
                return name_matches_parameter(expr_attribute.attr.as_str(), name);
            }
            _ => return false,
        }
    }
}

/// Returns `true` when `argument_name` case-insensitively matches the parameter
/// name, or has the parameter name as a full underscore-separated prefix or
/// suffix. The parameter name is accepted in its raw spelling; leading and
/// trailing underscores are ignored before matching.
fn name_matches_parameter(argument_name: &str, parameter_name: &str) -> bool {
    let argument_name = argument_name.to_lowercase();
    let parameter_name = parameter_name.trim_matches('_').to_lowercase();

    argument_name == parameter_name
        || argument_name
            .strip_prefix(parameter_name.as_str())
            .is_some_and(|suffix| suffix.starts_with('_'))
        || argument_name
            .strip_suffix(parameter_name.as_str())
            .is_some_and(|prefix| prefix.ends_with('_'))
}

/// Given a function call, check if the expression is the "same name"
/// as the function being called.
///
/// This allows us to filter out repetitive inlay hints like `x: T = T(...)`.
/// While still allowing non-trivial ones like `x: T[U] = T()`.
fn call_matches_name(expr: &Expr, name: &str) -> bool {
    // Only care about function calls
    let Expr::Call(call) = expr else {
        return false;
    };

    match &*call.func {
        // `x: T = T()` is a match
        Expr::Name(expr_name) => expr_name.id.as_str() == name,
        // `x: T = a.T()` is a match
        Expr::Attribute(expr_attribute) => expr_attribute.attr.as_str() == name,
        _ => false,
    }
}

/// basedpython: whether the parser built `function` from a construct that spells
/// the signature somewhere else — a property accessor, whose parameter list stands
/// for no source at all.
///
/// Every real `def` and `lambda` writes its own parameter list, even an empty one,
/// so a list with no range is always synthesized.
fn has_synthesized_header(function: &ast::StmtFunctionDef) -> bool {
    function.parameters.range().is_empty()
}

/// Given an expression that's the RHS of an assignment, would it be excessive to
/// emit an inlay type hint for the variable assigned to it?
///
/// This is used to suppress inlay hints for things like `x = 1`, `x, y = (1, 2)`, etc.
fn type_hint_is_excessive_for_expr(expr: &Expr) -> bool {
    match expr {
        // A tuple of all literals is excessive to typehint
        Expr::Tuple(expr_tuple) => expr_tuple.elts.iter().all(type_hint_is_excessive_for_expr),

        // Various Literal[...] types which are always excessive to hint
        Expr::BytesLiteral(_)
        | Expr::NumberLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::StringLiteral(_) => true,
        // `None` isn't terribly verbose, but still redundant
        Expr::NoneLiteral(_) => true,
        // This one expands to `str` which isn't verbose but is redundant
        Expr::FString(_) => true,
        // This one expands to `Template` which isn't verbose but is redundant
        Expr::TString(_) => true,

        // You too `+1 and `-1`, get back here
        Expr::UnaryOp(ExprUnaryOp {
            op: UnaryOp::UAdd | UnaryOp::USub,
            operand,
            ..
        }) => matches!(**operand, Expr::NumberLiteral(_)),

        // Everything else is reasonable
        _ => false,
    }
}

fn should_skip_import(db: &dyn Db, module: ty_module_resolver::Module, ty: Type) -> bool {
    module.is_known(db, ty_module_resolver::KnownModule::Builtins) || ty.is_none(db)
}

/// basedpython: the initializer of a declaration that names no type — `let a = v`,
/// `var a = v`, `context a = v` and the modifier chains that lower like `var`
///
/// the parser models a declaration as an annotated assignment whose annotation is
/// a synthetic marker spanning the keyword text. a declaration that *does* name a
/// type keeps it under the marker — `let a: T = v` parses as `a: __let__[T] = v`
/// — so a bare marker is exactly what says the type is unwritten, and the name is
/// where writing it would go
fn untyped_declaration_value(assign: &ast::StmtAnnAssign) -> Option<&Expr> {
    let Expr::Name(marker) = assign.annotation.as_ref() else {
        return None;
    };
    if !marker.ctx.is_invalid() {
        return None;
    }
    matches!(
        marker.id.as_str(),
        "__let__" | "__modifier_assign__" | "__context__"
    )
    .then(|| assign.value.as_deref())
    .flatten()
}

fn annotations_are_valid_syntax(stmt_assign: &ruff_python_ast::StmtAssign) -> bool {
    if stmt_assign.targets.len() > 1 {
        return false;
    }

    if stmt_assign
        .targets
        .iter()
        .any(|target| matches!(target, Expr::Tuple(_)))
    {
        return false;
    }

    true
}

fn is_ignored_variable_assignment_target(expr: &Expr) -> bool {
    let Expr::Name(name) = expr else {
        return false;
    };

    let name = name.id.as_str();
    let is_dunder = name.starts_with("__") && name.ends_with("__") && name.len() > 4;

    name.starts_with('_') && !is_dunder
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DynamicallyImportedMember {
    module: String,
    name: String,
}

struct DynamicImporter<'a, 'db> {
    importer: &'a Importer<'db>,
    /// The expression node used to compute members in scope (lazily).
    scope_node: AnyNodeRef<'a>,
    scope_offset: TextSize,
    members: Option<MembersInScope<'db>>,
    dynamic_imports: &'a mut FxHashMap<DynamicallyImportedMember, ImportAction>,
    imported_members: Vec<DynamicallyImportedMember>,
}

impl<'a, 'db> DynamicImporter<'a, 'db> {
    fn new(
        importer: &'a Importer<'db>,
        expr: &'a Expr,
        dynamic_imports: &'a mut FxHashMap<DynamicallyImportedMember, ImportAction>,
    ) -> Self {
        Self {
            importer,
            scope_node: expr.into(),
            scope_offset: expr.range().start(),
            members: None,
            dynamic_imports,
            imported_members: Vec::new(),
        }
    }

    /// Attempts to import a given symbol.
    /// If the symbol in the text edit needs to be qualified, we return the qualified symbol text.
    fn import_symbol(
        &mut self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        ty: &Type<'db>,
        module_name: &str,
        symbol_name: &str,
        label_text: &str,
    ) -> Option<String> {
        use std::collections::hash_map::Entry;

        // Ensure members are computed before borrowing other fields.
        let members = self.members.get_or_insert_with(|| {
            self.importer
                .members_in_scope_at(self.scope_node, self.scope_offset)
        });

        // Check if the label is like `foo.A`
        let mut is_possibly_qualified_name = label_text.contains('.');

        if let Some(member) = members.find_member(symbol_name) {
            if member.ty.definition(db, env) == ty.definition(db, env) {
                return None;
            }

            // There is another member in scope with the same name,
            // so we need to qualify this so we don't reference the
            // in scope member.
            is_possibly_qualified_name = true;
        }

        let key = DynamicallyImportedMember {
            module: module_name.to_string(),
            name: symbol_name.to_string(),
        };

        match self.dynamic_imports.entry(key.clone()) {
            Entry::Vacant(entry) => {
                let request = if is_possibly_qualified_name {
                    ImportRequest::import(module_name, symbol_name).force()
                } else {
                    ImportRequest::import_from(module_name, symbol_name)
                };

                let import_action = self.importer.import(request, members);
                let action = entry.insert(import_action);

                self.imported_members.push(key);

                qualified_symbol_text(action).map(str::to_string)
            }
            Entry::Occupied(entry) => qualified_symbol_text(entry.get()).map(str::to_string),
        }
    }

    /// Builds the text edits from all collected imports.
    fn text_edits(&self) -> Vec<InlayHintTextEdit> {
        self.imported_members
            .iter()
            .filter_map(|member| self.dynamic_imports.get(member))
            .filter_map(|import_action| {
                import_action.import().and_then(|edit| {
                    edit.content().map(|content| InlayHintTextEdit {
                        range: edit.range(),
                        new_text: content.to_string(),
                    })
                })
            })
            .collect()
    }
}

/// If the import action requires qualifying the symbol (e.g. `import foo` instead of
/// `from foo import A`), returns the qualified symbol text. Otherwise returns `None`.
fn qualified_symbol_text(import_action: &ImportAction) -> Option<&str> {
    if import_action.import().is_some() {
        return None;
    }
    Some(import_action.symbol_text())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::NavigationTarget;
    use crate::tests::{IntoDiagnostic, diagnostic_touches_vendored_file};
    use insta::{assert_snapshot, internals::SettingsBindDropGuard};
    use itertools::Itertools;
    use ruff_db::{
        diagnostic::{
            Annotation, Diagnostic, DiagnosticFormat, DiagnosticId, DisplayDiagnosticConfig,
            LintName, Severity, Span, SubDiagnostic, SubDiagnosticSeverity,
        },
        files::{File, FileRange, system_path_to_file},
        source::source_text,
    };
    use ruff_diagnostics::{Edit, Fix};
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;
    use ruff_python_trivia::textwrap::dedent;
    use ruff_text_size::{TextLen, TextSize};

    use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
    use ty_project::ProjectMetadata;

    pub(super) fn inlay_hint_test(source: &str) -> InlayHintTest {
        inlay_hint_test_in("main.py", source, false)
    }

    /// Like [`inlay_hint_test`], but for a `.by` source, so basedpython-only
    /// hints are produced.
    pub(super) fn basedpython_inlay_hint_test(source: &str) -> InlayHintTest {
        inlay_hint_test_in("main.by", source, false)
    }

    /// An inlay-hint test with `analysis.sound-types` enabled, for the signatures ty
    /// recovers rather than reads.
    pub(super) fn sound_types_inlay_hint_test(source: &str) -> InlayHintTest {
        inlay_hint_test_in("main.by", source, true)
    }

    fn inlay_hint_test_in(file_name: &str, source: &str, sound_types: bool) -> InlayHintTest {
        const START: &str = "<START>";
        const END: &str = "<END>";

        let mut metadata = ProjectMetadata::new("test", SystemPathBuf::from("/"));
        if sound_types {
            metadata.apply_override_options(ty_project::metadata::Options {
                analysis: Some(ty_project::metadata::options::AnalysisOptions {
                    sound_types: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        let mut db = ty_project::TestDb::new(metadata);

        let source = dedent(source);

        let start = source.find(START);
        let end = source
            .find(END)
            .map(|x| if start.is_some() { x - START.len() } else { x })
            .unwrap_or(source.len());

        let range = TextRange::new(
            TextSize::try_from(start.unwrap_or_default()).unwrap(),
            TextSize::try_from(end).unwrap(),
        );

        let source = source.replace(START, "");
        let source = source.replace(END, "");

        db.write_file(file_name, source)
            .expect("write to memory file system to be successful");

        let file = system_path_to_file(&db, file_name).expect("newly written file to existing");

        let mut insta_settings = insta::Settings::clone_current();
        insta_settings.add_filter(r#"\\(\w\w|\.|")"#, "/$1");
        // Filter out TODO types because they are different between debug and release builds.
        insta_settings.add_filter(r"@Todo\(.+\)", "@Todo");

        let insta_settings_guard = insta_settings.bind_to_scope();

        InlayHintTest {
            db,
            file,
            range,
            _insta_settings_guard: insta_settings_guard,
        }
    }

    pub(super) struct InlayHintTest {
        db: ty_project::TestDb,
        file: File,
        range: TextRange,
        _insta_settings_guard: SettingsBindDropGuard,
    }

    impl InlayHintTest {
        /// Returns the inlay hints for the given test case.
        ///
        /// All inlay hints are generated using the applicable settings. Use
        /// [`inlay_hints_with_settings`] to generate hints with custom settings.
        ///
        /// [`inlay_hints_with_settings`]: Self::inlay_hints_with_settings
        fn inlay_hints(&mut self) -> String {
            self.inlay_hints_with_settings(&InlayHintSettings::default())
        }

        fn with_extra_file(&mut self, file_name: &str, content: &str) {
            self.db.write_file(file_name, content).unwrap();
        }

        /// Every hint as `«label»`, with a space the *client* draws written as
        /// `_` and a space the label itself carries left as a space.
        ///
        /// A snapshot of the file cannot tell the two apart, because both reach
        /// the reader as a space. This renders them apart, so that a label
        /// growing an edge space back is a test failure rather than a silent
        /// regression in what the hint asks the client to draw.
        fn padded_hints(&mut self, settings: &InlayHintSettings) -> String {
            inlay_hints(
                &self.db,
                ProgramFile::new(
                    &self.db,
                    self.file,
                    self.db.program_environment().program(&self.db),
                ),
                self.range,
                settings,
            )
            .into_iter()
            .map(|hint| {
                let label = hint
                    .label
                    .parts()
                    .iter()
                    .map(InlayHintLabelPart::text)
                    .join("");
                let padding = |padded: bool| if padded { "_" } else { "" };

                format!(
                    "«{}{label}{}»",
                    padding(hint.padding_left),
                    padding(hint.padding_right)
                )
            })
            .join("\n")
        }

        /// Returns the inlay hints for the given test case with custom settings.
        fn inlay_hints_with_settings(&mut self, settings: &InlayHintSettings) -> String {
            let hints = inlay_hints(
                &self.db,
                ProgramFile::new(
                    &self.db,
                    self.file,
                    self.db.program_environment().program(&self.db),
                ),
                self.range,
                settings,
            );

            let mut inlay_hint_buf = source_text(&self.db, self.file).as_str().to_string();
            let mut text_edit_buf = inlay_hint_buf.clone();
            let source_has_errors =
                parse_unchecked_source(&text_edit_buf, PySourceType::Python).has_invalid_syntax();

            let mut tbd_diagnostics = Vec::new();

            let mut offset = 0;

            let mut all_edits = Vec::new();

            for hint in hints {
                let end_position = hint.position.to_usize() + offset;
                let mut hint_str = "[".to_string();

                // padding is drawn by the client rather than carried in the
                // label, so a snapshot shows it the way a reader would see it
                if hint.padding_left {
                    hint_str.push(' ');
                }

                for part in hint.label.parts() {
                    if let Some(target) = part.target().cloned() {
                        let part_position = u32::try_from(end_position + hint_str.len()).unwrap();
                        let part_len = u32::try_from(part.text().len()).unwrap();
                        let label_range =
                            TextRange::at(TextSize::new(part_position), TextSize::new(part_len));
                        tbd_diagnostics.push((label_range, target));
                    }
                    hint_str.push_str(part.text());
                }

                all_edits.extend(hint.text_edits);

                if hint.padding_right {
                    hint_str.push(' ');
                }

                hint_str.push(']');
                offset += hint_str.len();

                inlay_hint_buf.insert_str(end_position, &hint_str);
            }
            let mut edit_offset = TextSize::default();

            for edit in all_edits.iter().sorted_by_key(|edit| edit.range.start()) {
                let updated_range = edit.range + edit_offset;
                text_edit_buf.replace_range(updated_range.to_std_range(), &edit.new_text);
                edit_offset += edit.new_text.text_len() - edit.range.len();
            }

            let edited = parse_unchecked_source(&text_edit_buf, PySourceType::Python);
            if edited.has_invalid_syntax() && !source_has_errors {
                let syntax_errors = edited.errors().iter().map(|error| &error.error).join("\n");

                panic!(
                    "Fixed source has a syntax error where the source document does not. This is a bug in one of the generated inlay hint edits:
{syntax_errors}
Source with applied edits:
{text_edit_buf}"
                );
            }
            self.db.write_file("main2.py", &inlay_hint_buf).unwrap();
            let inlayed_file =
                system_path_to_file(&self.db, "main2.py").expect("newly written file to existing");

            let location_diagnostics = tbd_diagnostics.into_iter().map(|(label_range, target)| {
                InlayHintLocationDiagnostic::new(FileRange::new(inlayed_file, label_range), &target)
            });

            let mut rendered_diagnostics = location_diagnostics
                .map(|diagnostic| self.render_diagnostic(diagnostic))
                .join("");

            if !rendered_diagnostics.is_empty() {
                rendered_diagnostics = format!(
                    "{}{}",
                    crate::MarkupKind::PlainText.horizontal_line(),
                    rendered_diagnostics
                        .strip_suffix("\n")
                        .unwrap_or(&rendered_diagnostics)
                );
            }

            let fixes = if let Some((first_edit, rest)) = all_edits.split_first() {
                let edit_diagnostic = InlayHintEditDiagnostic::new(self.file, first_edit, rest);
                let text_edit_buf = self.render_diagnostic(edit_diagnostic);

                format!(
                    "{}{}",
                    crate::MarkupKind::PlainText.horizontal_line(),
                    text_edit_buf
                )
            } else {
                String::new()
            };

            format!("{inlay_hint_buf}{rendered_diagnostics}{fixes}")
        }

        fn render_diagnostic<D>(&self, diagnostic: D) -> String
        where
            D: IntoDiagnostic,
        {
            use std::fmt::Write;

            let mut buf = String::new();

            let config = DisplayDiagnosticConfig::new("ty")
                .color(false)
                .context(0)
                .format(DiagnosticFormat::Full);

            let diag = diagnostic.into_diagnostic();
            let config =
                config.anonymized_line_numbers(diagnostic_touches_vendored_file(&self.db, &diag));
            write!(buf, "{}", diag.display(&self.db, &config)).unwrap();

            buf
        }
    }

    /// A hint that needs a space between itself and the source it sits beside
    /// asks the client for one instead of writing it into its label, so that the
    /// space is never part of a label part that links somewhere and never part
    /// of what a client measures as the hint's text.
    ///
    /// `_` marks a space the client draws; a bare space is one the label carries.
    #[test]
    fn a_hint_leaves_the_space_beside_it_to_the_client() {
        let mut test = basedpython_inlay_hint_test(
            "
            class Base:
                def f(self) -> None: ...

            class Derived(Base):
                def f(self):
                    raise TypeError

            class Source[T]:
                def get(self) -> T: ...

            def make[T]():
                return T()
            ",
        );

        assert_snapshot!(test.padded_hints(&InlayHintSettings {
            inferred_return_types: true,
            inferred_raises: true,
            inferred_variance: true,
            inferred_reification: true,
            inferred_override: true,
            ..InlayHintSettings::none()
        }), @r"
        «override_»
        «_raises TypeError»
        «out_»
        «reified_»
        «_-> T@make»
        ");
    }

    /// The same, for the hints a `.py` file gets: one shown at the end of a line
    /// and one shown between the operands of a union.
    #[test]
    fn a_python_hint_leaves_the_space_beside_it_to_the_client() {
        let mut test = inlay_hint_test(
            "
            def f(x: float | None) -> None:
                reveal_type(x)
            ",
        );

        assert_snapshot!(test.padded_hints(&InlayHintSettings {
            revealed_types: true,
            numeric_promotions: true,
            ..InlayHintSettings::none()
        }), @"
        «_| int»
        «_int | float | None»
        ");
    }

    /// A hint written *inside* a parameter list is padded on the side that meets
    /// a written parameter, and on neither side where the list's own `(` or `,`
    /// already spaces it.
    #[test]
    fn an_implicit_parameter_is_padded_where_it_meets_written_source() {
        let mut test = basedpython_inlay_hint_test(
            "
            def apply(fn: (int) -> None) -> None:
                fn(1)

            class C:
                init(a: int)

            class D:
                init()

            apply:
                print(it)
            ",
        );

        assert_snapshot!(test.padded_hints(&InlayHintSettings {
            implicit_parameters: true,
            implicit_self: true,
            ..InlayHintSettings::none()
        }), @r"
        «self,_»
        «self»
        «_it: int»
        ");
    }

    /// a forwarded parameter pack's halves render in the starred spelling the file is
    /// written in, not python's attribute one
    #[test]
    fn forwarded_parameter_pack_hints() {
        let mut test = basedpython_inlay_hint_test(
            "
            def deco[Parameters: (*: *, **: *), R](fn: (**Parameters) -> R):
                def inner(*args: *Parameters, **kwargs: **Parameters) -> R:
                    positional = args
                    keyword = kwargs
                    return fn(*args, **kwargs)

                return inner
            ",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def deco[Parameters: (*: *, **: *), R](fn: (**Parameters) -> R)[ -> def inner(**Parameters@deco) -> R@deco]:
            def inner(*args: *Parameters, **kwargs: **Parameters) -> R:
                positional[: *Parameters@deco] = args
                keyword[: **Parameters@deco] = kwargs
                return fn(*args, **kwargs)

            return inner
        ");
    }

    #[test]
    fn template_literal_type_hints() {
        let mut test = basedpython_inlay_hint_test(
            "
            def route(path: f\"/{str}\"):
                slug = path
            ",
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        def route(path: f"/{str}"):
            slug[: f"/{str}"] = path

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:14
           |
        LL |     slug[: f"/{str}"] = path
           |              ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:16
           |
        LL |     slug[: f"/{str}"] = path
           |                ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.by:1:1
          |
        2 | def route(path: f"/{str}"):
          -     slug = path
        3 +     slug: f"/{str}" = path
          |
        "#);
    }

    #[test]
    fn property_accessor_hints() {
        // an accessor's header is synthesized, so it takes no implicit-parameter
        // hint for the receiver it never spells nor for the name a `set` does; the
        // bodies are ordinary source and still hint
        let mut test = basedpython_inlay_hint_test(
            "
            def compute() -> int:
                return 1

            class A:
                var x: int
                    field = 10
                    get():
                        y = compute()
                        return y
                    set(value):
                        print(value)
            ",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def compute() -> int:
            return 1

        class A:
            var x: int
                field = 10
                get():
                    y[: int] = compute()
                    return y
                set(value):
                    print(value)

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:17
           |
        LL |             y[: int] = compute()
           |                 ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.by:1:1
           |
        8  |         get():
           -             y = compute()
        9  +             y: int = compute()
        10 |             return y
           |
        ");
    }

    #[test]
    fn test_assign_statement() {
        let mut test = inlay_hint_test(
            "
            def i(x: int, /) -> int:
                return x

            x = 1
            y = x
            z = i(1)
            w = z
            aa = b'foo'
            bb = aa
            ",
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        def i(x: int, /) -> int:
            return x

        x = 1
        y[: Literal[1]] = x
        z[: int] = i(1)
        w[: int] = z
        aa = b'foo'
        bb[: Literal[b"foo"]] = aa

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | y[: Literal[1]] = x
           |     ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:13
           |
        LL | y[: Literal[1]] = x
           |             ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | z[: int] = i(1)
           |     ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | w[: int] = z
           |     ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:6
           |
        LL | bb[: Literal[b"foo"]] = aa
           |      ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class bytes(Sequence[int]):
           |       ^^^^^
        info: Source
          --> main2.py:LL:14
           |
        LL | bb[: Literal[b"foo"]] = aa
           |              ^^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        1  + from typing import Literal
        2  |
        --------------------------------------------------------------------------------
        6  | x = 1
           - y = x
           - z = i(1)
           - w = z
        7  + y: Literal[1] = x
        8  + z: int = i(1)
        9  + w: int = z
        10 | aa = b'foo'
           - bb = aa
        11 + bb: Literal[b"foo"] = aa
           |
        "#);
    }

    #[test]
    fn test_unpacked_tuple_assignment() {
        let mut test = inlay_hint_test(
            "
            def i(x: int, /) -> int:
                return x
            def s(x: str, /) -> str:
                return x

            x1, y1 = (1, 'abc')
            x2, y2 = (x1, y1)
            x3, y3 = (i(1), s('abc'))
            x4, y4 = (x3, y3)
            ",
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        def i(x: int, /) -> int:
            return x
        def s(x: str, /) -> str:
            return x

        x1, y1 = (1, 'abc')
        x2[: Literal[1]], y2[: Literal["abc"]] = (x1, y1)
        x3[: int], y3[: str] = (i(1), s('abc'))
        x4[: int], y4[: str] = (x3, y3)

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:6
           |
        LL | x2[: Literal[1]], y2[: Literal["abc"]] = (x1, y1)
           |      ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:14
           |
        LL | x2[: Literal[1]], y2[: Literal["abc"]] = (x1, y1)
           |              ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:24
           |
        LL | x2[: Literal[1]], y2[: Literal["abc"]] = (x1, y1)
           |                        ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:32
           |
        LL | x2[: Literal[1]], y2[: Literal["abc"]] = (x1, y1)
           |                                ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:6
           |
        LL | x3[: int], y3[: str] = (i(1), s('abc'))
           |      ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:17
           |
        LL | x3[: int], y3[: str] = (i(1), s('abc'))
           |                 ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:6
           |
        LL | x4[: int], y4[: str] = (x3, y3)
           |      ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:17
           |
        LL | x4[: int], y4[: str] = (x3, y3)
           |                 ^^^
        "#);
    }

    #[test]
    fn test_starred_unpacked_tuple_assignment() {
        let mut test = inlay_hint_test(
            "
            def foo(x: tuple[int, ...]):
                (a, *b) = x
            ",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: tuple[int, ...]):
            (a[: int], *b[: list[int]]) = x

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:10
           |
        LL |     (a[: int], *b[: list[int]]) = x
           |          ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:21
           |
        LL |     (a[: int], *b[: list[int]]) = x
           |                     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:26
           |
        LL |     (a[: int], *b[: list[int]]) = x
           |                          ^^^
        ");
    }

    #[test]
    fn test_leading_underscore_variable_assignment_has_no_type_inlay_hint() {
        let mut test = inlay_hint_test(
            "
            def i(x: int, /) -> int:
                return x

            _ = i(1)
            _ignored = i(1)
            __ignored = i(1)
            ",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def i(x: int, /) -> int:
            return x

        _ = i(1)
        _ignored = i(1)
        __ignored = i(1)
        ");
    }

    #[test]
    fn test_leading_underscore_variable_in_tuple_assignment_has_no_type_inlay_hint() {
        let mut test = inlay_hint_test(
            "
            def i(x: int, /) -> int:
                return x
            def s(x: str, /) -> str:
                return x

            x, _ignored = (i(1), s('abc'))
            __ignored, y = (i(1), s('abc'))
            ",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def i(x: int, /) -> int:
            return x
        def s(x: str, /) -> str:
            return x

        x[: int], _ignored = (i(1), s('abc'))
        __ignored, y[: str] = (i(1), s('abc'))

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | x[: int], _ignored = (i(1), s('abc'))
           |     ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:16
           |
        LL | __ignored, y[: str] = (i(1), s('abc'))
           |                ^^^
        ");
    }

    #[test]
    fn test_dunder_variable_assignment_has_type_inlay_hint() {
        let mut test = inlay_hint_test(
            "
            def i(x: int, /) -> int:
                return x

            __special__ = i(1)
            ",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def i(x: int, /) -> int:
            return x

        __special__[: int] = i(1)

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:15
           |
        LL | __special__[: int] = i(1)
           |               ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        4 |
          - __special__ = i(1)
        5 + __special__: int = i(1)
          |
        ");
    }

    #[test]
    fn test_multiple_assignment() {
        let mut test = inlay_hint_test(
            "
            def i(x: int, /) -> int:
                return x
            def s(x: str, /) -> str:
                return x

            x1, y1 = 1, 'abc'
            x2, y2 = x1, y1
            x3, y3 = i(1), s('abc')
            x4, y4 = x3, y3
            ",
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        def i(x: int, /) -> int:
            return x
        def s(x: str, /) -> str:
            return x

        x1, y1 = 1, 'abc'
        x2[: Literal[1]], y2[: Literal["abc"]] = x1, y1
        x3[: int], y3[: str] = i(1), s('abc')
        x4[: int], y4[: str] = x3, y3

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:6
           |
        LL | x2[: Literal[1]], y2[: Literal["abc"]] = x1, y1
           |      ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:14
           |
        LL | x2[: Literal[1]], y2[: Literal["abc"]] = x1, y1
           |              ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:24
           |
        LL | x2[: Literal[1]], y2[: Literal["abc"]] = x1, y1
           |                        ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:32
           |
        LL | x2[: Literal[1]], y2[: Literal["abc"]] = x1, y1
           |                                ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:6
           |
        LL | x3[: int], y3[: str] = i(1), s('abc')
           |      ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:17
           |
        LL | x3[: int], y3[: str] = i(1), s('abc')
           |                 ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:6
           |
        LL | x4[: int], y4[: str] = x3, y3
           |      ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:17
           |
        LL | x4[: int], y4[: str] = x3, y3
           |                 ^^^
        "#);
    }

    #[test]
    fn test_tuple_assignment() {
        let mut test = inlay_hint_test(
            "
            def i(x: int, /) -> int:
                return x
            def s(x: str, /) -> str:
                return x

            x = (1, 'abc')
            y = x
            z = (i(1), s('abc'))
            w = z
            ",
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        def i(x: int, /) -> int:
            return x
        def s(x: str, /) -> str:
            return x

        x = (1, 'abc')
        y[: tuple[Literal[1], Literal["abc"]]] = x
        z[: tuple[int, str]] = (i(1), s('abc'))
        w[: tuple[int, str]] = z

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class tuple[out Element](Sequence[Element]):
           |       ^^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | y[: tuple[Literal[1], Literal["abc"]]] = x
           |     ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:11
           |
        LL | y[: tuple[Literal[1], Literal["abc"]]] = x
           |           ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:19
           |
        LL | y[: tuple[Literal[1], Literal["abc"]]] = x
           |                   ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:23
           |
        LL | y[: tuple[Literal[1], Literal["abc"]]] = x
           |                       ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:31
           |
        LL | y[: tuple[Literal[1], Literal["abc"]]] = x
           |                               ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class tuple[out Element](Sequence[Element]):
           |       ^^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | z[: tuple[int, str]] = (i(1), s('abc'))
           |     ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:11
           |
        LL | z[: tuple[int, str]] = (i(1), s('abc'))
           |           ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:16
           |
        LL | z[: tuple[int, str]] = (i(1), s('abc'))
           |                ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class tuple[out Element](Sequence[Element]):
           |       ^^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | w[: tuple[int, str]] = z
           |     ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:11
           |
        LL | w[: tuple[int, str]] = z
           |           ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:16
           |
        LL | w[: tuple[int, str]] = z
           |                ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        1  + from typing import Literal
        2  |
        --------------------------------------------------------------------------------
        8  | x = (1, 'abc')
           - y = x
           - z = (i(1), s('abc'))
           - w = z
        9  + y: tuple[Literal[1], Literal["abc"]] = x
        10 + z: tuple[int, str] = (i(1), s('abc'))
        11 + w: tuple[int, str] = z
           |
        "#);
    }

    #[test]
    fn test_nested_tuple_assignment() {
        let mut test = inlay_hint_test(
            "
            def i(x: int, /) -> int:
                return x
            def s(x: str, /) -> str:
                return x

            x1, (y1, z1) = (1, ('abc', 2))
            x2, (y2, z2) = (x1, (y1, z1))
            x3, (y3, z3) = (i(1), (s('abc'), i(2)))
            x4, (y4, z4) = (x3, (y3, z3))",
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        def i(x: int, /) -> int:
            return x
        def s(x: str, /) -> str:
            return x

        x1, (y1, z1) = (1, ('abc', 2))
        x2[: Literal[1]], (y2[: Literal["abc"]], z2[: Literal[2]]) = (x1, (y1, z1))
        x3[: int], (y3[: str], z3[: int]) = (i(1), (s('abc'), i(2)))
        x4[: int], (y4[: str], z4[: int]) = (x3, (y3, z3))
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:6
           |
        LL | x2[: Literal[1]], (y2[: Literal["abc"]], z2[: Literal[2]]) = (x1, (y1, z1))
           |      ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:14
           |
        LL | x2[: Literal[1]], (y2[: Literal["abc"]], z2[: Literal[2]]) = (x1, (y1, z1))
           |              ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:25
           |
        LL | x2[: Literal[1]], (y2[: Literal["abc"]], z2[: Literal[2]]) = (x1, (y1, z1))
           |                         ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:33
           |
        LL | x2[: Literal[1]], (y2[: Literal["abc"]], z2[: Literal[2]]) = (x1, (y1, z1))
           |                                 ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:47
           |
        LL | x2[: Literal[1]], (y2[: Literal["abc"]], z2[: Literal[2]]) = (x1, (y1, z1))
           |                                               ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:55
           |
        LL | x2[: Literal[1]], (y2[: Literal["abc"]], z2[: Literal[2]]) = (x1, (y1, z1))
           |                                                       ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:6
           |
        LL | x3[: int], (y3[: str], z3[: int]) = (i(1), (s('abc'), i(2)))
           |      ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:18
           |
        LL | x3[: int], (y3[: str], z3[: int]) = (i(1), (s('abc'), i(2)))
           |                  ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:29
           |
        LL | x3[: int], (y3[: str], z3[: int]) = (i(1), (s('abc'), i(2)))
           |                             ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:6
           |
        LL | x4[: int], (y4[: str], z4[: int]) = (x3, (y3, z3))
           |      ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:18
           |
        LL | x4[: int], (y4[: str], z4[: int]) = (x3, (y3, z3))
           |                  ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:29
           |
        LL | x4[: int], (y4[: str], z4[: int]) = (x3, (y3, z3))
           |                             ^^^
        "#);
    }

    #[test]
    fn test_assign_statement_with_type_annotation() {
        let mut test = inlay_hint_test(
            "
            def i(x: int, /) -> int:
                return x

            x: int = 1
            y = x
            z: int = i(1)
            w = z",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def i(x: int, /) -> int:
            return x

        x: int = 1
        y[: Literal[1]] = x
        z: int = i(1)
        w[: int] = z
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | y[: Literal[1]] = x
           |     ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:13
           |
        LL | y[: Literal[1]] = x
           |             ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | w[: int] = z
           |     ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        1 + from typing import Literal
        2 |
        --------------------------------------------------------------------------------
        6 | x: int = 1
          - y = x
        7 + y: Literal[1] = x
        8 | z: int = i(1)
          - w = z
        9 + w: int = z
          |
        ");
    }

    #[test]
    fn test_assign_statement_out_of_range() {
        let mut test = inlay_hint_test(
            "
            def i(x: int, /) -> int:
                return x
            <START>x = i(1)<END>
            z = x",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def i(x: int, /) -> int:
            return x
        x[: int] = i(1)
        z = x
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | x[: int] = i(1)
           |     ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        3 |     return x
          - x = i(1)
        4 + x: int = i(1)
        5 | z = x
          |
        ");
    }

    #[test]
    fn test_assign_attribute_of_instance() {
        let mut test = inlay_hint_test(
            "
            class A:
                def __init__(self, y):
                    self.x = int(1)
                    self.y = y

            a = A(2)
            a.y = int(3)
            ",
        );

        assert_snapshot!(test.inlay_hints(), @"

        class A:
            def __init__(self, y):
                self.x = int(1)
                self.y[: y@__init__] = y

        a = A([y=]2)
        a.y = int(3)

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:24
          |
        3 |     def __init__(self, y):
          |                        ^
        info: Source
         --> main2.py:7:8
          |
        7 | a = A([y=]2)
          |        ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        6 |
          - a = A(2)
        7 + a = A(y=2)
        8 | a.y = int(3)
          |
        ");
    }

    #[test]
    fn test_match_name_binding() {
        let mut test = inlay_hint_test(
            r#"
            def my_func(command: str):
                match command.split():
                    case ["get", ab]:
                        x = ab
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        def my_func(command: str):
            match command.split():
                case ["get", ab]:
                    x[: str] = ab

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:17
           |
        LL |             x[: str] = ab
           |                 ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        4 |         case ["get", ab]:
          -             x = ab
        5 +             x: str = ab
          |
        "#);
    }

    #[test]
    fn test_match_rest_binding() {
        let mut test = inlay_hint_test(
            r#"
            def my_func(command: str):
                match command.split():
                    case ["get", *ab]:
                        x = ab
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        def my_func(command: str):
            match command.split():
                case ["get", *ab]:
                    x[: list[str]] = ab

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:17
           |
        LL |             x[: list[str]] = ab
           |                 ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:22
           |
        LL |             x[: list[str]] = ab
           |                      ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        4 |         case ["get", *ab]:
          -             x = ab
        5 +             x: list[str] = ab
          |
        "#);
    }

    #[test]
    fn test_match_as_binding() {
        let mut test = inlay_hint_test(
            r#"
            def my_func(command: str):
                match command.split():
                    case ["get", ("a" | "b") as ab]:
                        x = ab
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        def my_func(command: str):
            match command.split():
                case ["get", ("a" | "b") as ab]:
                    x[: Literal["a", "b"]] = ab

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:17
           |
        LL |             x[: Literal["a", "b"]] = ab
           |                 ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:25
           |
        LL |             x[: Literal["a", "b"]] = ab
           |                         ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:30
           |
        LL |             x[: Literal["a", "b"]] = ab
           |                              ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        1 + from typing import Literal
        2 |
        3 | def my_func(command: str):
        4 |     match command.split():
        5 |         case ["get", ("a" | "b") as ab]:
          -             x = ab
        6 +             x: Literal["a", "b"] = ab
          |
        "#);
    }

    #[test]
    fn test_match_keyword_binding() {
        let mut test = inlay_hint_test(
            r#"
            class Click:
                __match_args__ = ("position", "button")
                def __init__(self, pos, btn):
                    self.position: int = pos
                    self.button: str = btn

            def my_func(event: Click):
                match event:
                    case Click(x, button=ab):
                        x = ab
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        class Click:
            __match_args__ = ("position", "button")
            def __init__(self, pos, btn):
                self.position: int = pos
                self.button: str = btn

        def my_func(event: Click):
            match event:
                case Click(x, button=ab):
                    x[: str] = ab

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:17
           |
        LL |             x[: str] = ab
           |                 ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        10 |         case Click(x, button=ab):
           -             x = ab
        11 +             x: str = ab
           |
        "#);
    }

    #[test]
    fn test_typevar_name_binding() {
        let mut test = inlay_hint_test(
            r#"
            type Alias1[AB: int = bool] = tuple[AB, list[AB]]
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        type Alias1[AB: int = bool] = tuple[AB, list[AB]]
        ");
    }

    #[test]
    fn test_typevar_spec_binding() {
        let mut test = inlay_hint_test(
            r#"
            from typing import Callable
            type Alias2[**AB = [int, str]] = Callable[AB, tuple[AB]]
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        from typing import Callable
        type Alias2[**AB = [int, str]] = Callable[AB, tuple[AB]]
        ");
    }

    #[test]
    fn test_typevar_tuple_binding() {
        let mut test = inlay_hint_test(
            r#"
            type Alias3[*AB = ()] = tuple[tuple[*AB], tuple[*AB]]
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        type Alias3[*AB = ()] = tuple[tuple[*AB], tuple[*AB]]
        ");
    }

    #[test]
    fn test_many_literals() {
        let mut test = inlay_hint_test(
            r#"
            a = 1
            b = 1.0
            c = True
            d = None
            e = "hello"
            f = 'there'
            g = f"{e} {f}"
            h = t"wow %d"
            i = b'\x00'
            j = +1
            k = -1.0
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        a = 1
        b = 1.0
        c = True
        d = None
        e = "hello"
        f = 'there'
        g = f"{e} {f}"
        h = t"wow %d"
        i = b'/x00'
        j = +1
        k = -1.0
        "#);
    }

    #[test]
    fn test_many_literals_tuple() {
        let mut test = inlay_hint_test(
            r#"
            a = (1, 2)
            b = (1.0, 2.0)
            c = (True, False)
            d = (None, None)
            e = ("hel", "lo")
            f = ('the', 're')
            g = (f"{ft}", f"{ft}")
            h = (t"wow %d", t"wow %d")
            i = (b'\x01', b'\x02')
            j = (+1, +2.0)
            k = (-1, -2.0)
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        a = (1, 2)
        b = (1.0, 2.0)
        c = (True, False)
        d = (None, None)
        e = ("hel", "lo")
        f = ('the', 're')
        g = (f"{ft}", f"{ft}")
        h = (t"wow %d", t"wow %d")
        i = (b'/x01', b'/x02')
        j = (+1, +2.0)
        k = (-1, -2.0)
        "#);
    }

    #[test]
    fn test_many_literals_unpacked_tuple() {
        let mut test = inlay_hint_test(
            r#"
            a1, a2 = (1, 2)
            b1, b2 = (1.0, 2.0)
            c1, c2 = (True, False)
            d1, d2 = (None, None)
            e1, e2 = ("hel", "lo")
            f1, f2 = ('the', 're')
            g1, g2 = (f"{ft}", f"{ft}")
            h1, h2 = (t"wow %d", t"wow %d")
            i1, i2 = (b'\x01', b'\x02')
            j1, j2 = (+1, +2.0)
            k1, k2 = (-1, -2.0)
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        a1, a2 = (1, 2)
        b1, b2 = (1.0, 2.0)
        c1, c2 = (True, False)
        d1, d2 = (None, None)
        e1, e2 = ("hel", "lo")
        f1, f2 = ('the', 're')
        g1, g2 = (f"{ft}", f"{ft}")
        h1, h2 = (t"wow %d", t"wow %d")
        i1, i2 = (b'/x01', b'/x02')
        j1, j2 = (+1, +2.0)
        k1, k2 = (-1, -2.0)
        "#);
    }

    #[test]
    fn test_many_literals_multiple() {
        let mut test = inlay_hint_test(
            r#"
            a1, a2 = 1, 2
            b1, b2 = 1.0, 2.0
            c1, c2 = True, False
            d1, d2 = None, None
            e1, e2 = "hel", "lo"
            f1, f2 = 'the', 're'
            g1, g2 = f"{ft}", f"{ft}"
            h1, h2 = t"wow %d", t"wow %d"
            i1, i2 = b'\x01', b'\x02'
            j1, j2 = +1, +2.0
            k1, k2 = -1, -2.0
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        a1, a2 = 1, 2
        b1, b2 = 1.0, 2.0
        c1, c2 = True, False
        d1, d2 = None, None
        e1, e2 = "hel", "lo"
        f1, f2 = 'the', 're'
        g1, g2 = f"{ft}", f"{ft}"
        h1, h2 = t"wow %d", t"wow %d"
        i1, i2 = b'/x01', b'/x02'
        j1, j2 = +1, +2.0
        k1, k2 = -1, -2.0
        "#);
    }

    #[test]
    fn test_many_literals_list() {
        let mut test = inlay_hint_test(
            r#"
            a = [1, 2]
            b = [1.0, 2.0]
            c = [True, False]
            d = [None, None]
            e = ["hel", "lo"]
            f = ['the', 're']
            g = [f"{ft}", f"{ft}"]
            h = [t"wow %d", t"wow %d"]
            i = [b'\x01', b'\x02']
            j = [+1, +2.0]
            k = [-1, -2.0]
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        a[: list[int]] = [1, 2]
        b[: list[int | float]] = [1.0, 2.0]
        c[: list[bool]] = [True, False]
        d[: list[None | Unknown]] = [None, None]
        e[: list[str]] = ["hel", "lo"]
        f[: list[str]] = ['the', 're']
        g[: list[str]] = [f"{ft}", f"{ft}"]
        h[: list[Template]] = [t"wow %d", t"wow %d"]
        i[: list[bytes]] = [b'/x01', b'/x02']
        j[: list[int | float]] = [+1, +2.0]
        k[: list[int | float]] = [-1, -2.0]

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | a[: list[int]] = [1, 2]
           |     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:10
           |
        LL | a[: list[int]] = [1, 2]
           |          ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | b[: list[int | float]] = [1.0, 2.0]
           |     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:10
           |
        LL | b[: list[int | float]] = [1.0, 2.0]
           |          ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class float:
           |       ^^^^^
        info: Source
          --> main2.py:LL:16
           |
        LL | b[: list[int | float]] = [1.0, 2.0]
           |                ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | c[: list[bool]] = [True, False]
           |     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:13
           |
        LL | final class bool(int):
           |             ^^^^
        info: Source
          --> main2.py:LL:10
           |
        LL | c[: list[bool]] = [True, False]
           |          ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | d[: list[None | Unknown]] = [None, None]
           |     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/types.byi:LL:13
           |
        LL | final class NoneType:
           |             ^^^^^^^^
        info: Source
          --> main2.py:LL:10
           |
        LL | d[: list[None | Unknown]] = [None, None]
           |          ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/ty_extensions/_internal.pyi:LL:1
           |
        LL | Unknown: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:17
           |
        LL | d[: list[None | Unknown]] = [None, None]
           |                 ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | e[: list[str]] = ["hel", "lo"]
           |     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:10
           |
        LL | e[: list[str]] = ["hel", "lo"]
           |          ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | f[: list[str]] = ['the', 're']
           |     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:10
           |
        LL | f[: list[str]] = ['the', 're']
           |          ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | g[: list[str]] = [f"{ft}", f"{ft}"]
           |     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:10
           |
        LL | g[: list[str]] = [f"{ft}", f"{ft}"]
           |          ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | h[: list[Template]] = [t"wow %d", t"wow %d"]
           |     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/string/templatelib.byi:LL:13
           |
        LL | final class Template:  # TODO: consider making `Template` generic on `TypeVarTuple`
           |             ^^^^^^^^
        info: Source
          --> main2.py:LL:10
           |
        LL | h[: list[Template]] = [t"wow %d", t"wow %d"]
           |          ^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | i[: list[bytes]] = [b'/x01', b'/x02']
           |     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class bytes(Sequence[int]):
           |       ^^^^^
        info: Source
          --> main2.py:LL:10
           |
        LL | i[: list[bytes]] = [b'/x01', b'/x02']
           |          ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | j[: list[int | float]] = [+1, +2.0]
           |     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:10
           |
        LL | j[: list[int | float]] = [+1, +2.0]
           |          ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class float:
           |       ^^^^^
        info: Source
          --> main2.py:LL:16
           |
        LL | j[: list[int | float]] = [+1, +2.0]
           |                ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | k[: list[int | float]] = [-1, -2.0]
           |     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:10
           |
        LL | k[: list[int | float]] = [-1, -2.0]
           |          ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class float:
           |       ^^^^^
        info: Source
          --> main2.py:LL:16
           |
        LL | k[: list[int | float]] = [-1, -2.0]
           |                ^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        1  + from ty_extensions._internal import Unknown
        2  + from string.templatelib import Template
        3  |
           - a = [1, 2]
           - b = [1.0, 2.0]
           - c = [True, False]
           - d = [None, None]
           - e = ["hel", "lo"]
           - f = ['the', 're']
           - g = [f"{ft}", f"{ft}"]
           - h = [t"wow %d", t"wow %d"]
           - i = [b'/x01', b'/x02']
           - j = [+1, +2.0]
           - k = [-1, -2.0]
        4  + a: list[int] = [1, 2]
        5  + b: list[int | float] = [1.0, 2.0]
        6  + c: list[bool] = [True, False]
        7  + d: list[None | Unknown] = [None, None]
        8  + e: list[str] = ["hel", "lo"]
        9  + f: list[str] = ['the', 're']
        10 + g: list[str] = [f"{ft}", f"{ft}"]
        11 + h: list[Template] = [t"wow %d", t"wow %d"]
        12 + i: list[bytes] = [b'/x01', b'/x02']
        13 + j: list[int | float] = [+1, +2.0]
        14 + k: list[int | float] = [-1, -2.0]
           |
        "#);
    }

    /// an `auto()` member leaves its value to the enum, so the value it hands
    /// out is shown where the call stands in for it
    #[test]
    fn enum_auto_values() {
        let mut test = inlay_hint_test(
            r#"
            from enum import Enum, auto

            class Color(Enum):
                RED = auto()
                GREEN = 7
                BLUE = auto()
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        from enum import Enum, auto

        class Color(Enum):
            RED = auto()[ 1]
            GREEN = 7
            BLUE = auto()[ 2]
        ");
    }

    /// annotating an enum member is an error, and ty reads the value off the
    /// annotation rather than the `auto()` — so there is nothing to show
    #[test]
    fn annotated_enum_auto_values_are_not_hinted() {
        let mut test = inlay_hint_test(
            r#"
            from enum import Enum, auto

            class Color(Enum):
                RED: int = auto()
                BLUE: int = auto()
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        from enum import Enum, auto

        class Color(Enum):
            RED: int = auto()
            BLUE: int = auto()
        ");
    }

    /// a `StrEnum`'s `auto()` names the member rather than counting
    #[test]
    fn enum_auto_string_values() {
        let mut test = inlay_hint_test(
            r#"
            from enum import StrEnum, auto

            class Color(StrEnum):
                RED = auto()
                BLUE = auto()
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        from enum import StrEnum, auto

        class Color(StrEnum):
            RED = auto()[ "red"]
            BLUE = auto()[ "blue"]
        "#);
    }

    /// a `Flag`'s `auto()` doubles rather than counts, so its members are shown
    /// the bits they set
    #[test]
    fn enum_flag_auto_values() {
        let mut test = inlay_hint_test(
            r#"
            from enum import Flag, IntFlag, auto

            class Style(Flag):
                BOLD = auto()
                ITALIC = auto()
                UNDERLINE = auto()

            class Perm(IntFlag):
                READ = auto()
                WRITE = auto()
                EXECUTE = auto()
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        from enum import Flag, IntFlag, auto

        class Style(Flag):
            BOLD = auto()[ 1]
            ITALIC = auto()[ 2]
            UNDERLINE = auto()[ 4]

        class Perm(IntFlag):
            READ = auto()[ 1]
            WRITE = auto()[ 2]
            EXECUTE = auto()[ 4]
        ");
    }

    /// an enum whose mixin's `auto()` behaviour ty cannot follow has no value to
    /// show, only the type one would have
    #[test]
    fn enum_auto_values_of_an_unmodelled_mixin() {
        let mut test = inlay_hint_test(
            r#"
            from enum import Enum, auto

            class Color(bytes, Enum):
                RED = auto()
                BLUE = auto()
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        from enum import Enum, auto

        class Color(bytes, Enum):
            RED = auto()
            BLUE = auto()
        ");
    }

    /// a `case` variant has nowhere to write a value, so the one the lowering
    /// counts out is shown after the name
    #[test]
    fn based_enum_case_values() {
        let mut test = basedpython_inlay_hint_test(
            "
            enum class Color:
                case Red, Green
                case Blue
            ",
        );

        assert_snapshot!(test.inlay_hints(), @"

        enum class Color:
            case Red[ 1], Green[ 2]
            case Blue[ 3]
        ");
    }

    /// a payload-bearing enum lowers to a sealed hierarchy, where a unit variant
    /// is a singleton of its own subclass rather than a counted-out value
    #[test]
    fn payload_enum_case_values_are_not_hinted() {
        let mut test = basedpython_inlay_hint_test(
            "
            enum class Shape:
                case Circle(radius: int)
                case Point
            ",
        );

        assert_snapshot!(test.inlay_hints(), @"

        enum class Shape:
            case Circle(radius: int)
            case Point
        ");
    }

    #[test]
    fn test_enum_literal() {
        let mut test = inlay_hint_test(
            r#"
            from enum import Enum

            class Color(Enum):
                RED = 1
                BLUE = 2

            x = Color.RED
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        from enum import Enum

        class Color(Enum):
            RED = 1
            BLUE = 2

        x[: Literal[Color.RED]] = Color.RED

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | x[: Literal[Color.RED]] = Color.RED
           |     ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:4:7
          |
        4 | class Color(Enum):
          |       ^^^^^
        info: Source
         --> main2.py:8:13
          |
        8 | x[: Literal[Color.RED]] = Color.RED
          |             ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:5:5
          |
        5 |     RED = 1
          |     ^^^
        info: Source
         --> main2.py:8:19
          |
        8 | x[: Literal[Color.RED]] = Color.RED
          |                   ^^^
        ");
    }

    #[test]
    fn test_simple_init_call() {
        let mut test = inlay_hint_test(
            r#"
            class MyClass:
                def __init__(self):
                    self.x: int = 1

            x = MyClass()
            y = (MyClass(), MyClass())
            a, b = MyClass(), MyClass()
            c, d = (MyClass(), MyClass())
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        class MyClass:
            def __init__(self):
                self.x: int = 1

        x = MyClass()
        y[: tuple[MyClass, MyClass]] = (MyClass(), MyClass())
        a[: MyClass], b[: MyClass] = MyClass(), MyClass()
        c[: MyClass], d[: MyClass] = (MyClass(), MyClass())

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class tuple[out Element](Sequence[Element]):
           |       ^^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | y[: tuple[MyClass, MyClass]] = (MyClass(), MyClass())
           |     ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | class MyClass:
          |       ^^^^^^^
        info: Source
         --> main2.py:7:11
          |
        7 | y[: tuple[MyClass, MyClass]] = (MyClass(), MyClass())
          |           ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | class MyClass:
          |       ^^^^^^^
        info: Source
         --> main2.py:7:20
          |
        7 | y[: tuple[MyClass, MyClass]] = (MyClass(), MyClass())
          |                    ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | class MyClass:
          |       ^^^^^^^
        info: Source
         --> main2.py:8:5
          |
        8 | a[: MyClass], b[: MyClass] = MyClass(), MyClass()
          |     ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | class MyClass:
          |       ^^^^^^^
        info: Source
         --> main2.py:8:19
          |
        8 | a[: MyClass], b[: MyClass] = MyClass(), MyClass()
          |                   ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | class MyClass:
          |       ^^^^^^^
        info: Source
         --> main2.py:9:5
          |
        9 | c[: MyClass], d[: MyClass] = (MyClass(), MyClass())
          |     ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | class MyClass:
          |       ^^^^^^^
        info: Source
         --> main2.py:9:19
          |
        9 | c[: MyClass], d[: MyClass] = (MyClass(), MyClass())
          |                   ^^^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        6 | x = MyClass()
          - y = (MyClass(), MyClass())
        7 + y: tuple[MyClass, MyClass] = (MyClass(), MyClass())
        8 | a, b = MyClass(), MyClass()
          |
        ");
    }

    #[test]
    fn test_generic_init_call() {
        let mut test = inlay_hint_test(
            r#"
            class MyClass[T, U]:
                def __init__(self, x: list[T], y: tuple[U, U]):
                    self.x = x
                    self.y = y

            x = MyClass([42], ("a", "b"))
            y = (MyClass([42], ("a", "b")), MyClass([42], ("a", "b")))
            a, b = MyClass([42], ("a", "b")), MyClass([42], ("a", "b"))
            c, d = (MyClass([42], ("a", "b")), MyClass([42], ("a", "b")))
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        class MyClass[T, U]:
            def __init__(self, x: list[T], y: tuple[U, U]):
                self.x[: list[T@MyClass]] = x
                self.y[: tuple[U@MyClass, U@MyClass]] = y

        x[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b"))
        y[: tuple[MyClass[int, str], MyClass[int, str]]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a", "b")))
        a[: MyClass[int, str]], b[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a", "b"))
        c[: MyClass[int, str]], d[: MyClass[int, str]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a", "b")))

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:18
           |
        LL |         self.x[: list[T@MyClass]] = x
           |                  ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class tuple[out Element](Sequence[Element]):
           |       ^^^^^
        info: Source
          --> main2.py:LL:18
           |
        LL |         self.y[: tuple[U@MyClass, U@MyClass]] = y
           |                  ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | class MyClass[T, U]:
          |       ^^^^^^^
        info: Source
         --> main2.py:7:5
          |
        7 | x[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b"))
          |     ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:13
           |
        LL | x[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b"))
           |             ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:18
           |
        LL | x[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b"))
           |                  ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:35
           |
        LL | x[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b"))
           |                                   ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:40
           |
        LL | x[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b"))
           |                                        ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:24
          |
        3 |     def __init__(self, x: list[T], y: tuple[U, U]):
          |                        ^
        info: Source
         --> main2.py:7:47
          |
        7 | x[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b"))
          |                                               ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:36
          |
        3 |     def __init__(self, x: list[T], y: tuple[U, U]):
          |                                    ^
        info: Source
         --> main2.py:7:57
          |
        7 | x[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b"))
          |                                                         ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class tuple[out Element](Sequence[Element]):
           |       ^^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | y[: tuple[MyClass[int, str], MyClass[int, str]]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=](…
           |     ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | class MyClass[T, U]:
          |       ^^^^^^^
        info: Source
         --> main2.py:8:11
          |
        8 | y[: tuple[MyClass[int, str], MyClass[int, str]]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("…
          |           ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:19
           |
        LL | y[: tuple[MyClass[int, str], MyClass[int, str]]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=](…
           |                   ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:24
           |
        LL | y[: tuple[MyClass[int, str], MyClass[int, str]]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=](…
           |                        ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | class MyClass[T, U]:
          |       ^^^^^^^
        info: Source
         --> main2.py:8:30
          |
        8 | y[: tuple[MyClass[int, str], MyClass[int, str]]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("…
          |                              ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:38
           |
        LL | y[: tuple[MyClass[int, str], MyClass[int, str]]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=](…
           |                                      ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:43
           |
        LL | y[: tuple[MyClass[int, str], MyClass[int, str]]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=](…
           |                                           ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:62
           |
        LL | y[: tuple[MyClass[int, str], MyClass[int, str]]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=](…
           |                                                              ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:67
           |
        LL | y[: tuple[MyClass[int, str], MyClass[int, str]]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=](…
           |                                                                   ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:24
          |
        3 |     def __init__(self, x: list[T], y: tuple[U, U]):
          |                        ^
        info: Source
         --> main2.py:8:74
          |
        8 | y[: tuple[MyClass[int, str], MyClass[int, str]]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("…
          |                                                                          ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:36
          |
        3 |     def __init__(self, x: list[T], y: tuple[U, U]):
          |                                    ^
        info: Source
         --> main2.py:8:84
          |
        8 | y[: tuple[MyClass[int, str], MyClass[int, str]]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("…
          |                                                                                    ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:109
           |
        LL | y[: tuple[MyClass[int, str], MyClass[int, str]]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=](…
           |                                                                                                             ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:114
           |
        LL | y[: tuple[MyClass[int, str], MyClass[int, str]]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=](…
           |                                                                                                                  ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:24
          |
        3 |     def __init__(self, x: list[T], y: tuple[U, U]):
          |                        ^
        info: Source
         --> main2.py:8:121
          |
        8 | y[: tuple[MyClass[int, str], MyClass[int, str]]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("…
          |                                                                                                                         ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:36
          |
        3 |     def __init__(self, x: list[T], y: tuple[U, U]):
          |                                    ^
        info: Source
         --> main2.py:8:131
          |
        8 | …, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a", "b")))
          |                                                                    ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | class MyClass[T, U]:
          |       ^^^^^^^
        info: Source
         --> main2.py:9:5
          |
        9 | a[: MyClass[int, str]], b[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a",…
          |     ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:13
           |
        LL | a[: MyClass[int, str]], b[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a"…
           |             ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:18
           |
        LL | a[: MyClass[int, str]], b[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a"…
           |                  ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | class MyClass[T, U]:
          |       ^^^^^^^
        info: Source
         --> main2.py:9:29
          |
        9 | a[: MyClass[int, str]], b[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a",…
          |                             ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:37
           |
        LL | a[: MyClass[int, str]], b[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a"…
           |                                     ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:42
           |
        LL | a[: MyClass[int, str]], b[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a"…
           |                                          ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:59
           |
        LL | a[: MyClass[int, str]], b[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a"…
           |                                                           ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:64
           |
        LL | a[: MyClass[int, str]], b[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a"…
           |                                                                ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:24
          |
        3 |     def __init__(self, x: list[T], y: tuple[U, U]):
          |                        ^
        info: Source
         --> main2.py:9:71
          |
        9 | a[: MyClass[int, str]], b[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a",…
          |                                                                       ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:36
          |
        3 |     def __init__(self, x: list[T], y: tuple[U, U]):
          |                                    ^
        info: Source
         --> main2.py:9:81
          |
        9 | a[: MyClass[int, str]], b[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a",…
          |                                                                                 ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:106
           |
        LL | a[: MyClass[int, str]], b[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a"…
           |                                                                                                          ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:111
           |
        LL | a[: MyClass[int, str]], b[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a"…
           |                                                                                                               ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:24
          |
        3 |     def __init__(self, x: list[T], y: tuple[U, U]):
          |                        ^
        info: Source
         --> main2.py:9:118
          |
        9 | a[: MyClass[int, str]], b[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a",…
          |                                                                                                                      ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:36
          |
        3 |     def __init__(self, x: list[T], y: tuple[U, U]):
          |                                    ^
        info: Source
         --> main2.py:9:128
          |
        9 | a[: MyClass[int, str]], b[: MyClass[int, str]] = MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a",…
          |                                                                                                                                ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | class MyClass[T, U]:
          |       ^^^^^^^
        info: Source
          --> main2.py:10:5
           |
        10 | c[: MyClass[int, str]], d[: MyClass[int, str]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a…
           |     ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:13
           |
        LL | c[: MyClass[int, str]], d[: MyClass[int, str]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a…
           |             ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:18
           |
        LL | c[: MyClass[int, str]], d[: MyClass[int, str]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a…
           |                  ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | class MyClass[T, U]:
          |       ^^^^^^^
        info: Source
          --> main2.py:10:29
           |
        10 | c[: MyClass[int, str]], d[: MyClass[int, str]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a…
           |                             ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:37
           |
        LL | c[: MyClass[int, str]], d[: MyClass[int, str]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a…
           |                                     ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:42
           |
        LL | c[: MyClass[int, str]], d[: MyClass[int, str]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a…
           |                                          ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:60
           |
        LL | c[: MyClass[int, str]], d[: MyClass[int, str]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a…
           |                                                            ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:65
           |
        LL | c[: MyClass[int, str]], d[: MyClass[int, str]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a…
           |                                                                 ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:24
          |
        3 |     def __init__(self, x: list[T], y: tuple[U, U]):
          |                        ^
        info: Source
          --> main2.py:10:72
           |
        10 | c[: MyClass[int, str]], d[: MyClass[int, str]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a…
           |                                                                        ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:36
          |
        3 |     def __init__(self, x: list[T], y: tuple[U, U]):
          |                                    ^
        info: Source
          --> main2.py:10:82
           |
        10 | c[: MyClass[int, str]], d[: MyClass[int, str]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a…
           |                                                                                  ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:107
           |
        LL | c[: MyClass[int, str]], d[: MyClass[int, str]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a…
           |                                                                                                           ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:112
           |
        LL | c[: MyClass[int, str]], d[: MyClass[int, str]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a…
           |                                                                                                                ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:24
          |
        3 |     def __init__(self, x: list[T], y: tuple[U, U]):
          |                        ^
        info: Source
          --> main2.py:10:119
           |
        10 | c[: MyClass[int, str]], d[: MyClass[int, str]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a…
           |                                                                                                                       ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:36
          |
        3 |     def __init__(self, x: list[T], y: tuple[U, U]):
          |                                    ^
        info: Source
          --> main2.py:10:129
           |
        10 | c[: MyClass[int, str]], d[: MyClass[int, str]] = (MyClass[[int, str]]([x=][42], [y=]("a", "b")), MyClass[[int, str]]([x=][42], [y=]("a…
           |                                                                                                                                 ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        6  |
           - x = MyClass([42], ("a", "b"))
           - y = (MyClass([42], ("a", "b")), MyClass([42], ("a", "b")))
           - a, b = MyClass([42], ("a", "b")), MyClass([42], ("a", "b"))
           - c, d = (MyClass([42], ("a", "b")), MyClass([42], ("a", "b")))
        7  + x: MyClass[int, str] = MyClass([42], y=("a", "b"))
        8  + y: tuple[MyClass[int, str], MyClass[int, str]] = (MyClass([42], y=("a", "b")), MyClass([42], y=("a", "b")))
        9  + a, b = MyClass([42], y=("a", "b")), MyClass([42], y=("a", "b"))
        10 + c, d = (MyClass([42], y=("a", "b")), MyClass([42], y=("a", "b")))
           |
        "#);
    }

    #[test]
    fn test_disabled_variable_types() {
        let mut test = inlay_hint_test(
            "
            def i(x: int, /) -> int:
                return x

            x = i(1)
            ",
        );

        assert_snapshot!(
            test.inlay_hints_with_settings(&InlayHintSettings {
                variable_types: false,
                ..Default::default()
            }),
            @"

        def i(x: int, /) -> int:
            return x

        x = i(1)
        "
        );
    }

    #[test]
    fn test_function_call_with_positional_or_keyword_parameter() {
        let mut test = inlay_hint_test(
            "
            def foo(x: int): pass
            foo(1)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int): pass
        foo([x=]1)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(x: int): pass
          |         ^
        info: Source
         --> main2.py:3:6
          |
        3 | foo([x=]1)
          |      ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | def foo(x: int): pass
          - foo(1)
        3 + foo(x=1)
          |
        ");
    }

    #[test]
    fn test_function_call_with_positional_or_keyword_parameter_redundant_name() {
        let mut test = inlay_hint_test(
            "
            def foo(x: int): pass
            x = 1
            y = 2
            foo(x)
            foo(y)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int): pass
        x = 1
        y = 2
        foo(x)
        foo([x=]y)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(x: int): pass
          |         ^
        info: Source
         --> main2.py:6:6
          |
        6 | foo([x=]y)
          |      ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        5 | foo(x)
          - foo(y)
        6 + foo(x=y)
          |
        ");
    }

    #[test]
    fn test_function_call_with_positional_or_keyword_parameter_redundant_attribute() {
        let mut test = inlay_hint_test(
            "
            def foo(x: int): pass
            class MyClass:
                def __init__(self):
                    self.x: int = 1
                    self.y: int = 2
            val = MyClass()

            foo(val.x)
            foo(val.y)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int): pass
        class MyClass:
            def __init__(self):
                self.x: int = 1
                self.y: int = 2
        val = MyClass()

        foo(val.x)
        foo([x=]val.y)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(x: int): pass
          |         ^
        info: Source
          --> main2.py:10:6
           |
        10 | foo([x=]val.y)
           |      ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        9  | foo(val.x)
           - foo(val.y)
        10 + foo(x=val.y)
           |
        ");
    }

    #[test]
    fn test_function_call_with_positional_or_keyword_parameter_redundant_attribute_not() {
        // This one checks that we don't allow elide `x=` for `x.y`
        let mut test = inlay_hint_test(
            "
            def foo(x: int): pass
            class MyClass:
                def __init__(self):
                    self.x: int = 1
                    self.y: int = 2
            x = MyClass()

            foo(x.x)
            foo(x.y)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int): pass
        class MyClass:
            def __init__(self):
                self.x: int = 1
                self.y: int = 2
        x = MyClass()

        foo(x.x)
        foo([x=]x.y)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(x: int): pass
          |         ^
        info: Source
          --> main2.py:10:6
           |
        10 | foo([x=]x.y)
           |      ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        9  | foo(x.x)
           - foo(x.y)
        10 + foo(x=x.y)
           |
        ");
    }

    #[test]
    fn test_function_call_with_positional_or_keyword_parameter_redundant_call() {
        let mut test = inlay_hint_test(
            "
            def foo(x: int): pass
            class MyClass:
                def __init__(self):
                def x() -> int:
                    return 1
                def y() -> int:
                    return 2
            val = MyClass()

            foo(val.x())
            foo(val.y())",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int): pass
        class MyClass:
            def __init__(self):
            def x() -> int:
                return 1
            def y() -> int:
                return 2
        val = MyClass()

        foo(val.x())
        foo([x=]val.y())
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(x: int): pass
          |         ^
        info: Source
          --> main2.py:12:6
           |
        12 | foo([x=]val.y())
           |      ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        11 | foo(val.x())
           - foo(val.y())
        12 + foo(x=val.y())
           |
        ");
    }

    #[test]
    fn test_function_call_with_positional_or_keyword_parameter_redundant_complex() {
        let mut test = inlay_hint_test(
            "
            from typing import List

            def foo(x: int): pass
            class MyClass:
                def __init__(self):
                def x() -> List[int]:
                    return 1
                def y() -> List[int]:
                    return 2
            val = MyClass()

            foo(val.x()[0])
            foo(val.y()[1])",
        );

        assert_snapshot!(test.inlay_hints(), @"

        from typing import List

        def foo(x: int): pass
        class MyClass:
            def __init__(self):
            def x() -> List[int]:
                return 1
            def y() -> List[int]:
                return 2
        val = MyClass()

        foo(val.x()[0])
        foo([x=]val.y()[1])
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:4:9
          |
        4 | def foo(x: int): pass
          |         ^
        info: Source
          --> main2.py:14:6
           |
        14 | foo([x=]val.y()[1])
           |      ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        13 | foo(val.x()[0])
           - foo(val.y()[1])
        14 + foo(x=val.y()[1])
           |
        ");
    }

    #[test]
    fn test_function_call_with_positional_or_keyword_parameter_redundant_subscript() {
        let mut test = inlay_hint_test(
            "
            def foo(x: int): pass
            x = [1]
            y = [2]

            foo(x[0])
            foo(y[0])",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int): pass
        x[: list[int]] = [1]
        y[: list[int]] = [2]

        foo(x[0])
        foo([x=]y[0])
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | x[: list[int]] = [1]
           |     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:10
           |
        LL | x[: list[int]] = [1]
           |          ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | y[: list[int]] = [2]
           |     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:10
           |
        LL | y[: list[int]] = [2]
           |          ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(x: int): pass
          |         ^
        info: Source
         --> main2.py:7:6
          |
        7 | foo([x=]y[0])
          |      ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | def foo(x: int): pass
          - x = [1]
          - y = [2]
        3 + x: list[int] = [1]
        4 + y: list[int] = [2]
        5 |
        6 | foo(x[0])
          - foo(y[0])
        7 + foo(x=y[0])
          |
        ");
    }

    #[test]
    fn test_function_call_with_positional_only_parameter() {
        let mut test = inlay_hint_test(
            "
            def foo(x: int, /): pass
            foo(1)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int, /): pass
        foo(1)
        ");
    }

    #[test]
    fn test_function_call_with_variadic_parameter() {
        let mut test = inlay_hint_test(
            "
            def foo(*args: int): pass
            foo(1)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(*args: int): pass
        foo(1)
        ");
    }

    #[test]
    fn test_function_call_with_keyword_variadic_parameter() {
        let mut test = inlay_hint_test(
            "
            def foo(**kwargs: int): pass
            foo(x=1)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(**kwargs: int): pass
        foo(x=1)
        ");
    }

    #[test]
    fn test_function_call_with_keyword_only_parameter() {
        let mut test = inlay_hint_test(
            "
            def foo(*, x: int): pass
            foo(x=1)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(*, x: int): pass
        foo(x=1)
        ");
    }

    #[test]
    fn test_function_call_with_unpacked_tuple_argument() {
        // When an unpacked tuple fills multiple parameters, no hint should be shown
        // for that argument because showing a single parameter name would be misleading.
        let mut test = inlay_hint_test(
            "
            def foo(a: str, b: int, c: int, d: str): ...
            t: tuple[int, int] = (23, 42)
            foo('foo', *t, d='bar')",
        );

        // `*t` fills both `b` and `c`, so no hint is shown for it
        assert_snapshot!(test.inlay_hints(), @"

        def foo(a: str, b: int, c: int, d: str): ...
        t: tuple[int, int] = (23, 42)
        foo([a=]'foo', *t, d='bar')
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(a: str, b: int, c: int, d: str): ...
          |         ^
        info: Source
         --> main2.py:4:6
          |
        4 | foo([a=]'foo', *t, d='bar')
          |      ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        3 | t: tuple[int, int] = (23, 42)
          - foo('foo', *t, d='bar')
        4 + foo(a='foo', *t, d='bar')
          |
        ");
    }

    #[test]
    fn test_function_call_with_unpacked_tuple_argument_single_element() {
        // When an unpacked tuple fills only one parameter, a hint should be shown.
        let mut test = inlay_hint_test(
            "
            def foo(a: str, b: int, c: str): ...
            t: tuple[int] = (42,)
            foo('foo', *t, 'bar')",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(a: str, b: int, c: str): ...
        t: tuple[int] = (42,)
        foo([a=]'foo', [b=]*t, [c=]'bar')
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(a: str, b: int, c: str): ...
          |         ^
        info: Source
         --> main2.py:4:6
          |
        4 | foo([a=]'foo', [b=]*t, [c=]'bar')
          |      ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:17
          |
        2 | def foo(a: str, b: int, c: str): ...
          |                 ^
        info: Source
         --> main2.py:4:17
          |
        4 | foo([a=]'foo', [b=]*t, [c=]'bar')
          |                 ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:25
          |
        2 | def foo(a: str, b: int, c: str): ...
          |                         ^
        info: Source
         --> main2.py:4:25
          |
        4 | foo([a=]'foo', [b=]*t, [c=]'bar')
          |                         ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        3 | t: tuple[int] = (42,)
          - foo('foo', *t, 'bar')
        4 + foo('foo', *t, c='bar')
          |
        ");
    }

    #[test]
    fn test_function_call_last_plain_positional_before_starred_argument() {
        let mut test = inlay_hint_test(
            "
            def foo(a: int, b: int): ...
            t: tuple[int] = (2,)
            foo(1, *t)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(a: int, b: int): ...
        t: tuple[int] = (2,)
        foo([a=]1, [b=]*t)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(a: int, b: int): ...
          |         ^
        info: Source
         --> main2.py:4:6
          |
        4 | foo([a=]1, [b=]*t)
          |      ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:17
          |
        2 | def foo(a: int, b: int): ...
          |                 ^
        info: Source
         --> main2.py:4:13
          |
        4 | foo([a=]1, [b=]*t)
          |             ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        3 | t: tuple[int] = (2,)
          - foo(1, *t)
        4 + foo(a=1, *t)
          |
        ");
    }

    #[test]
    fn test_function_call_only_starred_argument_has_no_edit() {
        let mut test = inlay_hint_test(
            "
            def foo(a: int): ...
            t: tuple[int] = (1,)
            foo(*t)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(a: int): ...
        t: tuple[int] = (1,)
        foo([a=]*t)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(a: int): ...
          |         ^
        info: Source
         --> main2.py:4:6
          |
        4 | foo([a=]*t)
          |      ^
        ");
    }

    #[test]
    fn test_function_call_positional_only_and_positional_or_keyword_parameters() {
        let mut test = inlay_hint_test(
            "
            def foo(x: int, /, y: int): pass
            foo(1, 2)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int, /, y: int): pass
        foo(1, [y=]2)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:20
          |
        2 | def foo(x: int, /, y: int): pass
          |                    ^
        info: Source
         --> main2.py:3:9
          |
        3 | foo(1, [y=]2)
          |         ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | def foo(x: int, /, y: int): pass
          - foo(1, 2)
        3 + foo(1, y=2)
          |
        ");
    }

    #[test]
    fn test_function_call_positional_only_and_variadic_parameters() {
        let mut test = inlay_hint_test(
            "
            def foo(x: int, /, *args: int): pass
            foo(1, 2, 3)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int, /, *args: int): pass
        foo(1, 2, 3)
        ");
    }

    #[test]
    fn test_function_call_positional_only_and_keyword_variadic_parameters() {
        let mut test = inlay_hint_test(
            "
            def foo(x: int, /, **kwargs: int): pass
            foo(1, x=2)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int, /, **kwargs: int): pass
        foo(1, x=2)
        ");
    }

    #[test]
    fn test_class_constructor_call_init() {
        let mut test = inlay_hint_test(
            "
            class Foo:
                def __init__(self, x: int): pass
            Foo(1)
            f = Foo(1)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        class Foo:
            def __init__(self, x: int): pass
        Foo([x=]1)
        f = Foo([x=]1)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:24
          |
        3 |     def __init__(self, x: int): pass
          |                        ^
        info: Source
         --> main2.py:4:6
          |
        4 | Foo([x=]1)
          |      ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:24
          |
        3 |     def __init__(self, x: int): pass
          |                        ^
        info: Source
         --> main2.py:5:10
          |
        5 | f = Foo([x=]1)
          |          ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        3 |     def __init__(self, x: int): pass
          - Foo(1)
          - f = Foo(1)
        4 + Foo(x=1)
        5 + f = Foo(x=1)
          |
        ");
    }

    #[test]
    fn test_named_tuple_constructor_call() {
        let mut test = inlay_hint_test(
            "
            from typing import NamedTuple

            class Foo(NamedTuple):
                x: int
                y: str

            Foo(1, 'a')",
        );

        assert_snapshot!(test.inlay_hints(), @"

        from typing import NamedTuple

        class Foo(NamedTuple):
            x: int
            y: str

        Foo([x=]1, [y=]'a')
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:5:5
          |
        5 |     x: int
          |     ^
        info: Source
         --> main2.py:8:6
          |
        8 | Foo([x=]1, [y=]'a')
          |      ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:6:5
          |
        6 |     y: str
          |     ^
        info: Source
         --> main2.py:8:13
          |
        8 | Foo([x=]1, [y=]'a')
          |             ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        7 |
          - Foo(1, 'a')
        8 + Foo(1, y='a')
          |
        ");
    }

    #[test]
    fn test_class_constructor_call_new() {
        let mut test = inlay_hint_test(
            "
            class Foo:
                def __new__(cls, x: int): pass
            Foo(1)
            f = Foo(1)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        class Foo:
            def __new__(cls, x: int): pass
        Foo([x=]1)
        f = Foo([x=]1)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:22
          |
        3 |     def __new__(cls, x: int): pass
          |                      ^
        info: Source
         --> main2.py:4:6
          |
        4 | Foo([x=]1)
          |      ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:22
          |
        3 |     def __new__(cls, x: int): pass
          |                      ^
        info: Source
         --> main2.py:5:10
          |
        5 | f = Foo([x=]1)
          |          ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        3 |     def __new__(cls, x: int): pass
          - Foo(1)
          - f = Foo(1)
        4 + Foo(x=1)
        5 + f = Foo(x=1)
          |
        ");
    }

    #[test]
    fn test_class_constructor_call_meta_class_call() {
        let mut test = inlay_hint_test(
            "
            class MetaFoo:
                def __call__(self, x: int): pass
            class Foo(metaclass=MetaFoo):
                pass
            Foo(1)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        class MetaFoo:
            def __call__(self, x: int): pass
        class Foo(metaclass=MetaFoo):
            pass
        Foo([x=]1)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:24
          |
        3 |     def __call__(self, x: int): pass
          |                        ^
        info: Source
         --> main2.py:6:6
          |
        6 | Foo([x=]1)
          |      ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        5 |     pass
          - Foo(1)
        6 + Foo(x=1)
          |
        ");
    }

    #[test]
    fn test_callable_call() {
        let mut test = inlay_hint_test(
            "
            from typing import Callable
            def foo(x: Callable[[int], int]):
                x(1)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        from typing import Callable
        def foo(x: Callable[[int], int]):
            x(1)
        ");
    }

    #[test]
    fn test_instance_method_call() {
        let mut test = inlay_hint_test(
            "
            class Foo:
                def bar(self, y: int): pass
            Foo().bar(2)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        class Foo:
            def bar(self, y: int): pass
        Foo().bar([y=]2)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:19
          |
        3 |     def bar(self, y: int): pass
          |                   ^
        info: Source
         --> main2.py:4:12
          |
        4 | Foo().bar([y=]2)
          |            ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        3 |     def bar(self, y: int): pass
          - Foo().bar(2)
        4 + Foo().bar(y=2)
          |
        ");
    }

    #[test]
    fn instance_method_overload_self_type() {
        let mut test = inlay_hint_test(
            r#"
            from typing import overload

            class Parent:
                @overload
                def choose(self: "Child", child_value: int) -> None: ...
                @overload
                def choose(self: "Parent", parent_value: int) -> None: ...
                def choose(self, value: int) -> None: ...

            class Child(Parent): pass

            def f(parent: Parent, child: Child):
                parent.choose(1)
                child.choose(2)"#,
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        from typing import overload

        class Parent:
            @overload
            def choose(self: "Child", child_value: int) -> None: ...
            @overload
            def choose(self: "Parent", parent_value: int) -> None: ...
            def choose(self, value: int) -> None: ...

        class Child(Parent): pass

        def f(parent: Parent, child: Child):
            parent.choose([parent_value=]1)
            child.choose([child_value=]2)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:8:32
          |
        8 |     def choose(self: "Parent", parent_value: int) -> None: ...
          |                                ^^^^^^^^^^^^
        info: Source
          --> main2.py:14:20
           |
        14 |     parent.choose([parent_value=]1)
           |                    ^^^^^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:6:31
          |
        6 |     def choose(self: "Child", child_value: int) -> None: ...
          |                               ^^^^^^^^^^^
        info: Source
          --> main2.py:15:19
           |
        15 |     child.choose([child_value=]2)
           |                   ^^^^^^^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        13 | def f(parent: Parent, child: Child):
           -     parent.choose(1)
           -     child.choose(2)
        14 +     parent.choose(parent_value=1)
        15 +     child.choose(child_value=2)
           |
        "#);
    }

    #[test]
    fn test_class_method_call() {
        let mut test = inlay_hint_test(
            "
            class Foo:
                @classmethod
                def bar(cls, y: int): pass
            Foo.bar(2)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        class Foo:
            @classmethod
            def bar(cls, y: int): pass
        Foo.bar([y=]2)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:4:18
          |
        4 |     def bar(cls, y: int): pass
          |                  ^
        info: Source
         --> main2.py:5:10
          |
        5 | Foo.bar([y=]2)
          |          ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        4 |     def bar(cls, y: int): pass
          - Foo.bar(2)
        5 + Foo.bar(y=2)
          |
        ");
    }

    #[test]
    fn test_static_method_call() {
        let mut test = inlay_hint_test(
            "
            class Foo:
                @staticmethod
                def bar(y: int): pass
            Foo.bar(2)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        class Foo:
            @staticmethod
            def bar(y: int): pass
        Foo.bar([y=]2)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:4:13
          |
        4 |     def bar(y: int): pass
          |             ^
        info: Source
         --> main2.py:5:10
          |
        5 | Foo.bar([y=]2)
          |          ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        4 |     def bar(y: int): pass
          - Foo.bar(2)
        5 + Foo.bar(y=2)
          |
        ");
    }

    #[test]
    fn test_function_call_with_union_type() {
        let mut test = inlay_hint_test(
            "
            def foo(x: int | str): pass
            foo(1)
            foo('abc')",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int | str): pass
        foo([x=]1)
        foo([x=]'abc')
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(x: int | str): pass
          |         ^
        info: Source
         --> main2.py:3:6
          |
        3 | foo([x=]1)
          |      ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(x: int | str): pass
          |         ^
        info: Source
         --> main2.py:4:6
          |
        4 | foo([x=]'abc')
          |      ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | def foo(x: int | str): pass
          - foo(1)
          - foo('abc')
        3 + foo(x=1)
        4 + foo(x='abc')
          |
        ");
    }

    #[test]
    fn test_function_call_multiple_positional_arguments() {
        let mut test = inlay_hint_test(
            "
            def foo(x: int, y: str, z: bool): pass
            foo(1, 'hello', True)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int, y: str, z: bool): pass
        foo([x=]1, [y=]'hello', [z=]True)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(x: int, y: str, z: bool): pass
          |         ^
        info: Source
         --> main2.py:3:6
          |
        3 | foo([x=]1, [y=]'hello', [z=]True)
          |      ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:17
          |
        2 | def foo(x: int, y: str, z: bool): pass
          |                 ^
        info: Source
         --> main2.py:3:13
          |
        3 | foo([x=]1, [y=]'hello', [z=]True)
          |             ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:25
          |
        2 | def foo(x: int, y: str, z: bool): pass
          |                         ^
        info: Source
         --> main2.py:3:26
          |
        3 | foo([x=]1, [y=]'hello', [z=]True)
          |                          ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | def foo(x: int, y: str, z: bool): pass
          - foo(1, 'hello', True)
        3 + foo(1, 'hello', z=True)
          |
        ");
    }

    #[test]
    fn test_function_call_multiple_positional_arguments_before_keyword() {
        let mut test = inlay_hint_test(
            "
            def add(x: int, b, y: int) -> int:
                return x + y

            total = add(3, 2, y=4)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def add(x: int, b, y: int) -> int:
            return x + y

        total[: int] = add([x=]3, [b=]2, y=4)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:9
           |
        LL | total[: int] = add([x=]3, [b=]2, y=4)
           |         ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def add(x: int, b, y: int) -> int:
          |         ^
        info: Source
         --> main2.py:5:21
          |
        5 | total[: int] = add([x=]3, [b=]2, y=4)
          |                     ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:17
          |
        2 | def add(x: int, b, y: int) -> int:
          |                 ^
        info: Source
         --> main2.py:5:28
          |
        5 | total[: int] = add([x=]3, [b=]2, y=4)
          |                            ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        4 |
          - total = add(3, 2, y=4)
        5 + total: int = add(3, b=2, y=4)
          |
        ");
    }

    #[test]
    fn test_function_call_mixed_positional_and_keyword() {
        let mut test = inlay_hint_test(
            "
            def foo(x: int, y: str, z: bool): pass
            foo(1, z=True, y='hello')",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int, y: str, z: bool): pass
        foo([x=]1, z=True, y='hello')
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(x: int, y: str, z: bool): pass
          |         ^
        info: Source
         --> main2.py:3:6
          |
        3 | foo([x=]1, z=True, y='hello')
          |      ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | def foo(x: int, y: str, z: bool): pass
          - foo(1, z=True, y='hello')
        3 + foo(x=1, z=True, y='hello')
          |
        ");
    }

    #[test]
    fn test_function_call_positional_after_keyword_in_source_order() {
        // ty should continue to map positional args correctly in invalid or in-progress code,
        // even if a keyword arg appears earlier in source order.
        let mut test = inlay_hint_test(
            "
            def foo(x: int, y: str): pass
            foo(y='hello', 1)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int, y: str): pass
        foo(y='hello', [y=]1)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:17
          |
        2 | def foo(x: int, y: str): pass
          |                 ^
        info: Source
         --> main2.py:3:17
          |
        3 | foo(y='hello', [y=]1)
          |                 ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | def foo(x: int, y: str): pass
          - foo(y='hello', 1)
        3 + foo(y='hello', y=1)
          |
        ");
    }

    #[test]
    fn test_function_call_with_default_parameters() {
        let mut test = inlay_hint_test(
            "
            def foo(x: int, y: str = 'default', z: bool = False): pass
            foo(1)
            foo(1, 'custom')
            foo(1, 'custom', True)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int, y: str = 'default', z: bool = False): pass
        foo([x=]1)
        foo([x=]1, [y=]'custom')
        foo([x=]1, [y=]'custom', [z=]True)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(x: int, y: str = 'default', z: bool = False): pass
          |         ^
        info: Source
         --> main2.py:3:6
          |
        3 | foo([x=]1)
          |      ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(x: int, y: str = 'default', z: bool = False): pass
          |         ^
        info: Source
         --> main2.py:4:6
          |
        4 | foo([x=]1, [y=]'custom')
          |      ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:17
          |
        2 | def foo(x: int, y: str = 'default', z: bool = False): pass
          |                 ^
        info: Source
         --> main2.py:4:13
          |
        4 | foo([x=]1, [y=]'custom')
          |             ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(x: int, y: str = 'default', z: bool = False): pass
          |         ^
        info: Source
         --> main2.py:5:6
          |
        5 | foo([x=]1, [y=]'custom', [z=]True)
          |      ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:17
          |
        2 | def foo(x: int, y: str = 'default', z: bool = False): pass
          |                 ^
        info: Source
         --> main2.py:5:13
          |
        5 | foo([x=]1, [y=]'custom', [z=]True)
          |             ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:37
          |
        2 | def foo(x: int, y: str = 'default', z: bool = False): pass
          |                                     ^
        info: Source
         --> main2.py:5:27
          |
        5 | foo([x=]1, [y=]'custom', [z=]True)
          |                           ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | def foo(x: int, y: str = 'default', z: bool = False): pass
          - foo(1)
          - foo(1, 'custom')
          - foo(1, 'custom', True)
        3 + foo(x=1)
        4 + foo(1, y='custom')
        5 + foo(1, 'custom', z=True)
          |
        ");
    }

    #[test]
    fn test_nested_function_calls() {
        let mut test = inlay_hint_test(
            "
            def foo(x: int) -> int:
                return x * 2

            def bar(y: str) -> str:
                return y

            def baz(a: int, b: str, c: bool): pass

            baz(foo(5), bar(bar('test')), True)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int) -> int:
            return x * 2

        def bar(y: str) -> str:
            return y

        def baz(a: int, b: str, c: bool): pass

        baz([a=]foo([x=]5), [b=]bar([y=]bar([y=]'test')), [c=]True)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:8:9
          |
        8 | def baz(a: int, b: str, c: bool): pass
          |         ^
        info: Source
          --> main2.py:10:6
           |
        10 | baz([a=]foo([x=]5), [b=]bar([y=]bar([y=]'test')), [c=]True)
           |      ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(x: int) -> int:
          |         ^
        info: Source
          --> main2.py:10:14
           |
        10 | baz([a=]foo([x=]5), [b=]bar([y=]bar([y=]'test')), [c=]True)
           |              ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:8:17
          |
        8 | def baz(a: int, b: str, c: bool): pass
          |                 ^
        info: Source
          --> main2.py:10:22
           |
        10 | baz([a=]foo([x=]5), [b=]bar([y=]bar([y=]'test')), [c=]True)
           |                      ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:5:9
          |
        5 | def bar(y: str) -> str:
          |         ^
        info: Source
          --> main2.py:10:30
           |
        10 | baz([a=]foo([x=]5), [b=]bar([y=]bar([y=]'test')), [c=]True)
           |                              ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:5:9
          |
        5 | def bar(y: str) -> str:
          |         ^
        info: Source
          --> main2.py:10:38
           |
        10 | baz([a=]foo([x=]5), [b=]bar([y=]bar([y=]'test')), [c=]True)
           |                                      ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:8:25
          |
        8 | def baz(a: int, b: str, c: bool): pass
          |                         ^
        info: Source
          --> main2.py:10:52
           |
        10 | baz([a=]foo([x=]5), [b=]bar([y=]bar([y=]'test')), [c=]True)
           |                                                    ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        9  |
           - baz(foo(5), bar(bar('test')), True)
        10 + baz(foo(x=5), bar(y=bar(y='test')), c=True)
           |
        ");
    }

    #[test]
    fn test_method_chaining() {
        let mut test = inlay_hint_test(
            "
            class A:
                def foo(self, value: int) -> 'A':
                    return self
                def bar(self, name: str) -> 'A':
                    return self
                def baz(self): pass
            A().foo(42).bar('test').baz()",
        );

        assert_snapshot!(test.inlay_hints(), @"

        class A:
            def foo(self, value: int) -> 'A':
                return self
            def bar(self, name: str) -> 'A':
                return self
            def baz(self): pass
        A().foo([value=]42).bar([name=]'test').baz()
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:19
          |
        3 |     def foo(self, value: int) -> 'A':
          |                   ^^^^^
        info: Source
         --> main2.py:8:10
          |
        8 | A().foo([value=]42).bar([name=]'test').baz()
          |          ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:5:19
          |
        5 |     def bar(self, name: str) -> 'A':
          |                   ^^^^
        info: Source
         --> main2.py:8:26
          |
        8 | A().foo([value=]42).bar([name=]'test').baz()
          |                          ^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        7 |     def baz(self): pass
          - A().foo(42).bar('test').baz()
        8 + A().foo(value=42).bar(name='test').baz()
          |
        ");
    }

    #[test]
    fn test_nested_keyword_function_calls() {
        let mut test = inlay_hint_test(
            "
            def foo(x: str) -> str:
                return x
            def bar(y: int): pass
            bar(y=foo('test'))
            ",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: str) -> str:
            return x
        def bar(y: int): pass
        bar(y=foo([x=]'test'))

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(x: str) -> str:
          |         ^
        info: Source
         --> main2.py:5:12
          |
        5 | bar(y=foo([x=]'test'))
          |            ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        4 | def bar(y: int): pass
          - bar(y=foo('test'))
        5 + bar(y=foo(x='test'))
          |
        ");
    }

    #[test]
    fn test_lambda_function_calls() {
        let mut test = inlay_hint_test(
            "
            foo = lambda x: x * 2
            bar = lambda a, b: a + b
            foo(5)
            bar(1, 2)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        foo[: (x) -> Unknown] = lambda x: x * 2
        bar[: (a, b) -> Unknown] = lambda a, b: a + b
        foo([x=]5)
        bar([a=]1, [b=]2)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/ty_extensions/_internal.pyi:LL:1
           |
        LL | Unknown: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:14
           |
        LL | foo[: (x) -> Unknown] = lambda x: x * 2
           |              ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/ty_extensions/_internal.pyi:LL:1
           |
        LL | Unknown: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:17
           |
        LL | bar[: (a, b) -> Unknown] = lambda a, b: a + b
           |                 ^^^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        3 | bar = lambda a, b: a + b
          - foo(5)
          - bar(1, 2)
        4 + foo(x=5)
        5 + bar(1, b=2)
          |
        ");
    }

    #[test]
    fn test_literal_string() {
        let mut test = inlay_hint_test(
            r#"
            from typing import LiteralString
            def my_func(x: LiteralString):
                y = x
            my_func(x="hello")"#,
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        from typing import LiteralString
        def my_func(x: LiteralString):
            y[: LiteralString] = x
        my_func(x="hello")
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing_extensions.byi:LL:9
           |
        LL |         LiteralString,
           |         ^^^^^^^^^^^^^
        info: Source
          --> main2.py:LL:9
           |
        LL |     y[: LiteralString] = x
           |         ^^^^^^^^^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        3 | def my_func(x: LiteralString):
          -     y = x
        4 +     y: LiteralString = x
        5 | my_func(x="hello")
          |
        "#);
    }

    #[test]
    fn test_literal_group() {
        let mut test = inlay_hint_test(
            r#"
            def branch(cond: int):
                if cond < 10:
                    x = 1
                elif cond < 20:
                    x = 2
                elif cond < 30:
                    x = 3
                elif cond < 40:
                    x = "hello"
                else:
                    x = None
                y = x"#,
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        def branch(cond: int):
            if cond < 10:
                x = 1
            elif cond < 20:
                x = 2
            elif cond < 30:
                x = 3
            elif cond < 40:
                x = "hello"
            else:
                x = None
            y[: Literal[1, 2, 3, "hello"] | None] = x
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:9
           |
        LL |     y[: Literal[1, 2, 3, "hello"] | None] = x
           |         ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:17
           |
        LL |     y[: Literal[1, 2, 3, "hello"] | None] = x
           |                 ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:20
           |
        LL |     y[: Literal[1, 2, 3, "hello"] | None] = x
           |                    ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:23
           |
        LL |     y[: Literal[1, 2, 3, "hello"] | None] = x
           |                       ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:26
           |
        LL |     y[: Literal[1, 2, 3, "hello"] | None] = x
           |                          ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/types.byi:LL:13
           |
        LL | final class NoneType:
           |             ^^^^^^^^
        info: Source
          --> main2.py:LL:37
           |
        LL |     y[: Literal[1, 2, 3, "hello"] | None] = x
           |                                     ^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        1  + from typing import Literal
        2  |
        --------------------------------------------------------------------------------
        13 |         x = None
           -     y = x
        14 +     y: Literal[1, 2, 3, "hello"] | None = x
           |
        "#);
    }

    #[test]
    fn test_generic_alias() {
        let mut test = inlay_hint_test(
            r"
            class Foo[T]: ...

            a = Foo[int]",
        );

        assert_snapshot!(test.inlay_hints(), @"

        class Foo[T]: ...

        a[: <class 'Foo[int]'>] = Foo[int]
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | class Foo[T]: ...
          |       ^^^
        info: Source
         --> main2.py:4:13
          |
        4 | a[: <class 'Foo[int]'>] = Foo[int]
          |             ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:17
           |
        LL | a[: <class 'Foo[int]'>] = Foo[int]
           |                 ^^^
        ");
    }

    #[test]
    fn test_subclass_type() {
        let mut test = inlay_hint_test(
            r"
            def f(x: list[str]):
                y = type(x)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def f(x: list[str]):
            y[: type[list[str]]] = type(x)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class type:
           |       ^^^^
        info: Source
          --> main2.py:LL:9
           |
        LL |     y[: type[list[str]]] = type(x)
           |         ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:14
           |
        LL |     y[: type[list[str]]] = type(x)
           |              ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:19
           |
        LL |     y[: type[list[str]]] = type(x)
           |                   ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | def f(x: list[str]):
          -     y = type(x)
        3 +     y: type[list[str]] = type(x)
          |
        ");
    }

    #[test]
    fn test_property_literal_type() {
        let mut test = inlay_hint_test(
            r"
            class F:
                @property
                def whatever(self): ...

            ab = F.whatever",
        );

        assert_snapshot!(test.inlay_hints(), @"

        class F:
            @property
            def whatever(self): ...

        ab[: property] = F.whatever
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:4:9
          |
        4 |     def whatever(self): ...
          |         ^^^^^^^^
        info: Source
         --> main2.py:6:6
          |
        6 | ab[: property] = F.whatever
          |      ^^^^^^^^
        ");
    }

    #[test]
    fn test_complex_parameter_combinations() {
        let mut test = inlay_hint_test(
            "
            def foo(a: int, b: str, /, c: float, d: bool = True, *, e: int, f: str = 'default'): pass
            foo(1, 'pos', 3.14, False, e=42)
            foo(1, 'pos', 3.14, e=42, f='custom')",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(a: int, b: str, /, c: float[ | int], d: bool = True, *, e: int, f: str = 'default'): pass
        foo(1, 'pos', [c=]3.14, [d=]False, e=42)
        foo(1, 'pos', [c=]3.14, e=42, f='custom')
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:28
          |
        2 | def foo(a: int, b: str, /, c: float, d: bool = True, *, e: int, f: str = 'default'): pass
          |                            ^
        info: Source
         --> main2.py:3:16
          |
        3 | foo(1, 'pos', [c=]3.14, [d=]False, e=42)
          |                ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:38
          |
        2 | def foo(a: int, b: str, /, c: float, d: bool = True, *, e: int, f: str = 'default'): pass
          |                                      ^
        info: Source
         --> main2.py:3:26
          |
        3 | foo(1, 'pos', [c=]3.14, [d=]False, e=42)
          |                          ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:28
          |
        2 | def foo(a: int, b: str, /, c: float, d: bool = True, *, e: int, f: str = 'default'): pass
          |                            ^
        info: Source
         --> main2.py:4:16
          |
        4 | foo(1, 'pos', [c=]3.14, e=42, f='custom')
          |                ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | def foo(a: int, b: str, /, c: float, d: bool = True, *, e: int, f: str = 'default'): pass
          - foo(1, 'pos', 3.14, False, e=42)
          - foo(1, 'pos', 3.14, e=42, f='custom')
        3 + foo(1, 'pos', 3.14, d=False, e=42)
        4 + foo(1, 'pos', c=3.14, e=42, f='custom')
          |
        ");
    }

    #[test]
    fn test_function_calls_different_file() {
        let mut test = inlay_hint_test(
            "
            from foo import bar

            bar(1)",
        );

        test.with_extra_file(
            "foo.py",
            "
        def bar(x: int | str):
            pass",
        );

        assert_snapshot!(test.inlay_hints(), @"

        from foo import bar

        bar([x=]1)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> foo.py:2:17
          |
        2 |         def bar(x: int | str):
          |                 ^
        info: Source
         --> main2.py:4:6
          |
        4 | bar([x=]1)
          |      ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        3 |
          - bar(1)
        4 + bar(x=1)
          |
        ");
    }

    #[test]
    fn test_overloaded_function_calls() {
        let mut test = inlay_hint_test(
            "
            from typing import overload

            @overload
            def foo(x: int) -> str: ...
            @overload
            def foo(x: str) -> int: ...
            def foo(x):
                return x

            foo(42)
            foo('hello')",
        );

        assert_snapshot!(test.inlay_hints(), @"

        from typing import overload

        @overload
        def foo(x: int) -> str: ...
        @overload
        def foo(x: str) -> int: ...
        def foo(x[: int | str])[ -> str | int]:
            return x

        foo([x=]42)
        foo([x=]'hello')
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:5:9
          |
        5 | def foo(x: int) -> str: ...
          |         ^
        info: Source
          --> main2.py:11:6
           |
        11 | foo([x=]42)
           |      ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:7:9
          |
        7 | def foo(x: str) -> int: ...
          |         ^
        info: Source
          --> main2.py:12:6
           |
        12 | foo([x=]'hello')
           |      ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        10 |
           - foo(42)
           - foo('hello')
        11 + foo(x=42)
        12 + foo(x='hello')
           |
        ");
    }

    #[test]
    fn test_overloaded_function_calls_different_params() {
        let mut test = inlay_hint_test(
            "
            from typing import overload, Optional, Sequence

            @overload
            def S(name: str, is_symmetric: Optional[bool] = None) -> str: ...
            @overload
            def S(*names: str, is_symmetric: Optional[bool] = None) -> Sequence[str]: ...
            def S():
                pass

            b = S('x', 'y')",
        );

        // The call S('x', 'y') should match the second overload (*names: str),
        // and since *names is variadic, no parameter name hints should be shown.
        // Before the fix, this incorrectly showed `name=` and `is_symmetric=` hints
        // from the first overload.
        assert_snapshot!(test.inlay_hints(), @"

        from typing import overload, Optional, Sequence

        @overload
        def S(name: str, is_symmetric: Optional[bool] = None) -> str: ...
        @overload
        def S(*names: str, is_symmetric: Optional[bool] = None) -> Sequence[str]: ...
        def S()[ -> Sequence[str]]:
            pass

        b[: Sequence[str]] = S('x', 'y')
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/_collections_abc.byi:LL:7
           |
        LL | class Sequence[out Element](Reversible[Element], Collection[Element]):
           |       ^^^^^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | b[: Sequence[str]] = S('x', 'y')
           |     ^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:14
           |
        LL | b[: Sequence[str]] = S('x', 'y')
           |              ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        10 |
           - b = S('x', 'y')
        11 + b: Sequence[str] = S('x', 'y')
           |
        ");
    }

    #[test]
    fn test_overloaded_function_calls_no_matching_overload() {
        let mut test = inlay_hint_test(
            "
            from typing import overload

            @overload
            def f(x: int) -> str: ...
            @overload
            def f(x: str, y: str) -> int: ...
            def f(x):
                return x

            f([])
            ",
        );

        // Neither overload matches via type checking (list[Unknown] is neither int nor str),
        // so `matching_overloads()` returns empty. The arity-based fallback picks the first
        // overload (1 matched arg out of 1 required), and we should see the `x=` hint.
        assert_snapshot!(test.inlay_hints(), @"

        from typing import overload

        @overload
        def f(x: int) -> str: ...
        @overload
        def f(x: str, y: str) -> int: ...
        def f(x[: int | str])[ -> str | int]:
            return x

        f([x=][])

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:5:7
          |
        5 | def f(x: int) -> str: ...
          |       ^
        info: Source
          --> main2.py:11:4
           |
        11 | f([x=][])
           |    ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        10 |
           - f([])
        11 + f(x=[])
           |
        ");
    }

    #[test]
    fn test_disabled_function_argument_names() {
        let mut test = inlay_hint_test(
            "
        def foo(x: int): pass
        foo(1)",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            call_argument_names: false,
            ..Default::default()
        }), @"

        def foo(x: int): pass
        foo(1)
        ");
    }

    #[test]
    fn test_function_call_argument_name_suppressed_by_case_insensitive_exact_match() {
        let mut test = inlay_hint_test(
            "
            def foo(test: int, param: int): pass
            TEST = 1
            PARAM = 1

            foo(TEST, PARAM)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(test: int, param: int): pass
        TEST = 1
        PARAM = 1

        foo(TEST, PARAM)
        ");
    }

    #[test]
    fn test_function_call_argument_name_suppressed_by_normalized_parameter_name() {
        let mut test = inlay_hint_test(
            "
            def trailing(param_: int): pass
            def leading(_param: int): pass
            param = 1

            trailing(param)
            leading(param)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def trailing(param_: int): pass
        def leading(_param: int): pass
        param = 1

        trailing(param)
        leading(param)
        ");
    }

    #[test]
    fn test_function_call_argument_name_suppressed_by_segment_prefix_or_suffix() {
        let mut test = inlay_hint_test(
            "
            def foo(param: int): pass
            param = 1
            param_end = 1
            start_param = 1

            foo(param)
            foo(param_end)
            foo(start_param)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(param: int): pass
        param = 1
        param_end = 1
        start_param = 1

        foo(param)
        foo(param_end)
        foo(start_param)
        ");
    }

    #[test]
    fn test_function_call_argument_name_shown_for_near_matches() {
        let mut test = inlay_hint_test(
            "
            def foo(param: int): pass
            param2 = 1
            my_param2 = 1
            parameter = 1

            foo(param2)
            foo(my_param2)
            foo(parameter)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(param: int): pass
        param2 = 1
        my_param2 = 1
        parameter = 1

        foo([param=]param2)
        foo([param=]my_param2)
        foo([param=]parameter)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(param: int): pass
          |         ^^^^^
        info: Source
         --> main2.py:7:6
          |
        7 | foo([param=]param2)
          |      ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(param: int): pass
          |         ^^^^^
        info: Source
         --> main2.py:8:6
          |
        8 | foo([param=]my_param2)
          |      ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(param: int): pass
          |         ^^^^^
        info: Source
         --> main2.py:9:6
          |
        9 | foo([param=]parameter)
          |      ^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        6 |
          - foo(param2)
          - foo(my_param2)
          - foo(parameter)
        7 + foo(param=param2)
        8 + foo(param=my_param2)
        9 + foo(param=parameter)
          |
        ");
    }

    #[test]
    fn test_function_call_argument_name_suppression_matches_full_segment_sequence() {
        let mut test = inlay_hint_test(
            "
            def foo(focus_range: int): pass
            focus_range = 1
            FOCUS_RANGE = 1
            focus_range_end = 1
            start_focus_range = 1
            focus_end_range = 1

            foo(focus_range)
            foo(FOCUS_RANGE)
            foo(focus_range_end)
            foo(start_focus_range)
            foo(focus_end_range)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(focus_range: int): pass
        focus_range = 1
        FOCUS_RANGE = 1
        focus_range_end = 1
        start_focus_range = 1
        focus_end_range = 1

        foo(focus_range)
        foo(FOCUS_RANGE)
        foo(focus_range_end)
        foo(start_focus_range)
        foo([focus_range=]focus_end_range)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(focus_range: int): pass
          |         ^^^^^^^^^^^
        info: Source
          --> main2.py:13:6
           |
        13 | foo([focus_range=]focus_end_range)
           |      ^^^^^^^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        12 | foo(start_focus_range)
           - foo(focus_end_range)
        13 + foo(focus_range=focus_end_range)
           |
        ");
    }

    #[test]
    fn test_function_call_out_of_range() {
        let mut test = inlay_hint_test(
            "
            <START>def foo(x: int): pass
            def bar(y: int): pass
            foo(1)<END>
            bar(2)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int): pass
        def bar(y: int): pass
        foo([x=]1)
        bar(2)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:9
          |
        2 | def foo(x: int): pass
          |         ^
        info: Source
         --> main2.py:4:6
          |
        4 | foo([x=]1)
          |      ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        3 | def bar(y: int): pass
          - foo(1)
        4 + foo(x=1)
        5 | bar(2)
          |
        ");
    }

    #[test]
    fn test_function_call_with_argument_name_starting_with_underscore() {
        let mut test = inlay_hint_test(
            "
            def foo(_x: int, y: int): pass
            foo(1, 2)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(_x: int, y: int): pass
        foo(1, [y=]2)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:18
          |
        2 | def foo(_x: int, y: int): pass
          |                  ^
        info: Source
         --> main2.py:3:9
          |
        3 | foo(1, [y=]2)
          |         ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | def foo(_x: int, y: int): pass
          - foo(1, 2)
        3 + foo(1, y=2)
          |
        ");
    }

    #[test]
    fn test_function_call_different_formatting() {
        let mut test = inlay_hint_test(
            "
            def foo(
                x: int,
                y: int
            ): ...

            foo(1, 2)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(
            x: int,
            y: int
        ): ...

        foo([x=]1, [y=]2)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:5
          |
        3 |     x: int,
          |     ^
        info: Source
         --> main2.py:7:6
          |
        7 | foo([x=]1, [y=]2)
          |      ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:4:5
          |
        4 |     y: int
          |     ^
        info: Source
         --> main2.py:7:13
          |
        7 | foo([x=]1, [y=]2)
          |             ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        6 |
          - foo(1, 2)
        7 + foo(1, y=2)
          |
        ");
    }

    #[test]
    fn test_function_signature_inlay_hint() {
        let mut test = inlay_hint_test(
            "
        def foo(x: int, *y: bool, z: str | int | list[str]): ...

        a = foo",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def foo(x: int, *y: bool, z: str | int | list[str]): ...

        a[: def foo(x: int, *y: bool, *, z: str | int | list[str])] = foo
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:16
           |
        LL | a[: def foo(x: int, *y: bool, *, z: str | int | list[str])] = foo
           |                ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:13
           |
        LL | final class bool(int):
           |             ^^^^
        info: Source
          --> main2.py:LL:25
           |
        LL | a[: def foo(x: int, *y: bool, *, z: str | int | list[str])] = foo
           |                         ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:37
           |
        LL | a[: def foo(x: int, *y: bool, *, z: str | int | list[str])] = foo
           |                                     ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:43
           |
        LL | a[: def foo(x: int, *y: bool, *, z: str | int | list[str])] = foo
           |                                           ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:49
           |
        LL | a[: def foo(x: int, *y: bool, *, z: str | int | list[str])] = foo
           |                                                 ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:54
           |
        LL | a[: def foo(x: int, *y: bool, *, z: str | int | list[str])] = foo
           |                                                      ^^^
        ");
    }

    #[test]
    fn test_module_inlay_hint() {
        let mut test = inlay_hint_test(
            "
        import foo

        a = foo",
        );

        test.with_extra_file("foo.py", "'''Foo module'''");

        assert_snapshot!(test.inlay_hints(), @"

        import foo

        a[: <module 'foo'>] = foo
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/types.byi:LL:7
           |
        LL | class ModuleType:
           |       ^^^^^^^^^^
        info: Source
          --> main2.py:LL:6
           |
        LL | a[: <module 'foo'>] = foo
           |      ^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> foo.py:1:1
          |
        1 | '''Foo module'''
          | ^^^^^^^^^^^^^^^^
        info: Source
         --> main2.py:4:14
          |
        4 | a[: <module 'foo'>] = foo
          |              ^^^
        ");
    }

    #[test]
    fn test_literal_type_alias_inlay_hint() {
        let mut test = inlay_hint_test(
            "
        from typing import Literal

        a = Literal['a', 'b', 'c']",
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        from typing import Literal

        a[: <special-form 'Literal["a", "b", "c"]'>] = Literal['a', 'b', 'c']
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:20
           |
        LL | a[: <special-form 'Literal["a", "b", "c"]'>] = Literal['a', 'b', 'c']
           |                    ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:28
           |
        LL | a[: <special-form 'Literal["a", "b", "c"]'>] = Literal['a', 'b', 'c']
           |                            ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:33
           |
        LL | a[: <special-form 'Literal["a", "b", "c"]'>] = Literal['a', 'b', 'c']
           |                                 ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:38
           |
        LL | a[: <special-form 'Literal["a", "b", "c"]'>] = Literal['a', 'b', 'c']
           |                                      ^^^
        "#);
    }

    #[test]
    fn test_wrapper_descriptor_inlay_hint() {
        let mut test = inlay_hint_test(
            "
        from types import FunctionType

        a = FunctionType.__get__",
        );

        assert_snapshot!(test.inlay_hints(), @"

        from types import FunctionType

        a[: <wrapper-descriptor '__get__' of 'function' objects>] = FunctionType.__get__
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/types.byi:LL:13
           |
        LL | final class WrapperDescriptorType:
           |             ^^^^^^^^^^^^^^^^^^^^^
        info: Source
          --> main2.py:LL:6
           |
        LL | a[: <wrapper-descriptor '__get__' of 'function' objects>] = FunctionType.__get__
           |      ^^^^^^^^^^^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/types.byi:LL:13
           |
        LL | final class FunctionType:
           |             ^^^^^^^^^^^^
        info: Source
          --> main2.py:LL:39
           |
        LL | a[: <wrapper-descriptor '__get__' of 'function' objects>] = FunctionType.__get__
           |                                       ^^^^^^^^
        ");
    }

    #[test]
    fn test_method_wrapper_inlay_hint() {
        let mut test = inlay_hint_test(
            "
        def f(): ...

        a = f.__call__",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def f(): ...

        a[: <method-wrapper '__call__' of function 'f'>] = f.__call__
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/types.byi:LL:13
           |
        LL | final class MethodWrapperType:
           |             ^^^^^^^^^^^^^^^^^
        info: Source
          --> main2.py:LL:6
           |
        LL | a[: <method-wrapper '__call__' of function 'f'>] = f.__call__
           |      ^^^^^^^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/types.byi:LL:9
           |
        LL |     def __call__(self, *args: dynamic, **kwargs: dynamic) -> dynamic:
           |         ^^^^^^^^
        info: Source
          --> main2.py:LL:22
           |
        LL | a[: <method-wrapper '__call__' of function 'f'>] = f.__call__
           |                      ^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/types.byi:LL:13
           |
        LL | final class FunctionType:
           |             ^^^^^^^^^^^^
        info: Source
          --> main2.py:LL:35
           |
        LL | a[: <method-wrapper '__call__' of function 'f'>] = f.__call__
           |                                   ^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:5
          |
        2 | def f(): ...
          |     ^
        info: Source
         --> main2.py:4:45
          |
        4 | a[: <method-wrapper '__call__' of function 'f'>] = f.__call__
          |                                             ^
        ");
    }

    #[test]
    fn test_newtype_inlay_hint() {
        let mut test = inlay_hint_test(
            "
        from typing import NewType

        N = NewType('N', str)

        Y = N",
        );

        assert_snapshot!(test.inlay_hints(), @"

        from typing import NewType

        N[: <NewType pseudo-class 'N'>] = NewType([name=]'N', [tp=]str)

        Y[: <NewType pseudo-class 'N'>] = N
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:7
           |
        LL | class NewType:
           |       ^^^^^^^
        info: Source
          --> main2.py:LL:6
           |
        LL | N[: <NewType pseudo-class 'N'>] = NewType([name=]'N', [tp=]str)
           |      ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:4:1
          |
        4 | N = NewType('N', str)
          | ^
        info: Source
         --> main2.py:4:28
          |
        4 | N[: <NewType pseudo-class 'N'>] = NewType([name=]'N', [tp=]str)
          |                            ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:16
           |
        LL |     init(self, name: str, tp: dynamic)  # AnnotationForm
           |                ^^^^
        info: Source
          --> main2.py:LL:44
           |
        LL | N[: <NewType pseudo-class 'N'>] = NewType([name=]'N', [tp=]str)
           |                                            ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:27
           |
        LL |     init(self, name: str, tp: dynamic)  # AnnotationForm
           |                           ^^
        info: Source
          --> main2.py:LL:56
           |
        LL | N[: <NewType pseudo-class 'N'>] = NewType([name=]'N', [tp=]str)
           |                                                        ^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:7
           |
        LL | class NewType:
           |       ^^^^^^^
        info: Source
          --> main2.py:LL:6
           |
        LL | Y[: <NewType pseudo-class 'N'>] = N
           |      ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:4:1
          |
        4 | N = NewType('N', str)
          | ^
        info: Source
         --> main2.py:6:28
          |
        6 | Y[: <NewType pseudo-class 'N'>] = N
          |                            ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        3 |
          - N = NewType('N', str)
        4 + N = NewType('N', tp=str)
        5 |
          |
        ");
    }

    #[test]
    fn test_meta_typevar_inlay_hint() {
        let mut test = inlay_hint_test(
            "
        def f[T](x: type[T]):
            y = x",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def f[T](x: type[T]):
            y[: type[T@f]] = x
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class type:
           |       ^^^^
        info: Source
          --> main2.py:LL:9
           |
        LL |     y[: type[T@f]] = x
           |         ^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | def f[T](x: type[T]):
          |       ^
        info: Source
         --> main2.py:3:14
          |
        3 |     y[: type[T@f]] = x
          |              ^^^
        ");
    }

    #[test]
    fn test_subscripted_protocol_inlay_hint() {
        let mut test = inlay_hint_test(
            "
        from typing import Protocol, TypeVar
        T = TypeVar('T')
        Strange = Protocol[T]",
        );

        assert_snapshot!(test.inlay_hints(), @"

        from typing import Protocol, TypeVar
        T = TypeVar([name=]'T')
        Strange[: <special-form 'typing.Protocol[T]'>] = Protocol[T]
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:13
           |
        LL |             name: str,
           |             ^^^^
        info: Source
          --> main2.py:LL:14
           |
        LL | T = TypeVar([name=]'T')
           |              ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Protocol: _SpecialForm
           | ^^^^^^^^
        info: Source
          --> main2.py:LL:26
           |
        LL | Strange[: <special-form 'typing.Protocol[T]'>] = Protocol[T]
           |                          ^^^^^^^^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:1
          |
        3 | T = TypeVar('T')
          | ^
        info: Source
         --> main2.py:4:42
          |
        4 | Strange[: <special-form 'typing.Protocol[T]'>] = Protocol[T]
          |                                          ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | from typing import Protocol, TypeVar
          - T = TypeVar('T')
        3 + T = TypeVar(name='T')
        4 | Strange = Protocol[T]
          |
        ");
    }

    #[test]
    fn test_paramspec_creation_inlay_hint() {
        let mut test = inlay_hint_test(
            "
        from typing import ParamSpec
        P = ParamSpec('P')",
        );

        assert_snapshot!(test.inlay_hints(), @"

        from typing import ParamSpec
        P = ParamSpec([name=]'P')
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:13
           |
        LL |             name: str,
           |             ^^^^
        info: Source
          --> main2.py:LL:16
           |
        LL | P = ParamSpec([name=]'P')
           |                ^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | from typing import ParamSpec
          - P = ParamSpec('P')
        3 + P = ParamSpec(name='P')
          |
        ");
    }

    #[test]
    fn test_typealiastype_creation_inlay_hint() {
        let mut test = inlay_hint_test(
            "
        from typing_extensions import TypeAliasType
        A = TypeAliasType('A', str)",
        );

        assert_snapshot!(test.inlay_hints(), @"

        from typing_extensions import TypeAliasType
        A = TypeAliasType([name=]'A', [value=]str)
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:26
           |
        LL |         def __new__(cls, name: str, value: dynamic, *, type_params: (*: _TypeParameter) = ()) -> Self
           |                          ^^^^
        info: Source
          --> main2.py:LL:20
           |
        LL | A = TypeAliasType([name=]'A', [value=]str)
           |                    ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:37
           |
        LL |         def __new__(cls, name: str, value: dynamic, *, type_params: (*: _TypeParameter) = ()) -> Self
           |                                     ^^^^^
        info: Source
          --> main2.py:LL:32
           |
        LL | A = TypeAliasType([name=]'A', [value=]str)
           |                                ^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | from typing_extensions import TypeAliasType
          - A = TypeAliasType('A', str)
        3 + A = TypeAliasType('A', value=str)
          |
        ");
    }

    #[test]
    fn test_typevartuple_creation_inlay_hint() {
        let mut test = inlay_hint_test(
            "
        from typing_extensions import TypeVarTuple
        Ts = TypeVarTuple('Ts')",
        );

        assert_snapshot!(test.inlay_hints(), @"

        from typing_extensions import TypeVarTuple
        Ts = TypeVarTuple([name=]'Ts')
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing_extensions.byi:LL:17
           |
        LL |                 name: str,
           |                 ^^^^
        info: Source
          --> main2.py:LL:20
           |
        LL | Ts = TypeVarTuple([name=]'Ts')
           |                    ^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | from typing_extensions import TypeVarTuple
          - Ts = TypeVarTuple('Ts')
        3 + Ts = TypeVarTuple(name='Ts')
          |
        ");
    }

    #[test]
    fn hover_type_with_top_materialization() {
        let mut test = inlay_hint_test(
            r#"
                from typing import Any
                from ty_extensions import Top

                def f(xyxy: Top[list[Any]]):
                    x = xyxy
                "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        from typing import Any
        from ty_extensions import Top

        def f(xyxy: Top[list[Any]]):
            x[: Top[list[Any]]] = xyxy

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/ty_extensions/__init__.pyi:LL:1
           |
        LL | Top: _SpecialForm
           | ^^^
        info: Source
          --> main2.py:LL:9
           |
        LL |     x[: Top[list[Any]]] = xyxy
           |         ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:13
           |
        LL |     x[: Top[list[Any]]] = xyxy
           |             ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:7
           |
        LL | class Any:
           |       ^^^
        info: Source
          --> main2.py:LL:18
           |
        LL |     x[: Top[list[Any]]] = xyxy
           |                  ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        5 | def f(xyxy: Top[list[Any]]):
          -     x = xyxy
        6 +     x: Top[list[Any]] = xyxy
          |
        ");
    }

    #[test]
    fn test_auto_import_with_qualification_of_names() {
        let mut test = inlay_hint_test(
            "
            import foo

            a = foo.C().foo()
            ",
        );

        test.with_extra_file(
            "foo.py",
            "
            import bar

            class A[T]: ...

            class B[T]: ...

            class C:
                def foo(self) -> B[A[bar.D[int, list[str | A[B[int]]]]]]:
                    raise NotImplementedError
                    ",
        );

        test.with_extra_file(
            "bar.py",
            "
            class D[T, U]: ...
            ",
        );

        assert_snapshot!(test.inlay_hints(), @"

        import foo

        a[: B[A[D[int, list[str | A[B[int]]]]]]] = foo.C().foo()

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> foo.py:6:19
          |
        6 |             class B[T]: ...
          |                   ^
        info: Source
         --> main2.py:4:5
          |
        4 | a[: B[A[D[int, list[str | A[B[int]]]]]]] = foo.C().foo()
          |     ^

        info[inlay-hint-location]: Inlay Hint Target
         --> foo.py:4:19
          |
        4 |             class A[T]: ...
          |                   ^
        info: Source
         --> main2.py:4:7
          |
        4 | a[: B[A[D[int, list[str | A[B[int]]]]]]] = foo.C().foo()
          |       ^

        info[inlay-hint-location]: Inlay Hint Target
         --> bar.py:2:19
          |
        2 |             class D[T, U]: ...
          |                   ^
        info: Source
         --> main2.py:4:9
          |
        4 | a[: B[A[D[int, list[str | A[B[int]]]]]]] = foo.C().foo()
          |         ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:11
           |
        LL | a[: B[A[D[int, list[str | A[B[int]]]]]]] = foo.C().foo()
           |           ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:16
           |
        LL | a[: B[A[D[int, list[str | A[B[int]]]]]]] = foo.C().foo()
           |                ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:21
           |
        LL | a[: B[A[D[int, list[str | A[B[int]]]]]]] = foo.C().foo()
           |                     ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> foo.py:4:19
          |
        4 |             class A[T]: ...
          |                   ^
        info: Source
         --> main2.py:4:27
          |
        4 | a[: B[A[D[int, list[str | A[B[int]]]]]]] = foo.C().foo()
          |                           ^

        info[inlay-hint-location]: Inlay Hint Target
         --> foo.py:6:19
          |
        6 |             class B[T]: ...
          |                   ^
        info: Source
         --> main2.py:4:29
          |
        4 | a[: B[A[D[int, list[str | A[B[int]]]]]]] = foo.C().foo()
          |                             ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:31
           |
        LL | a[: B[A[D[int, list[str | A[B[int]]]]]]] = foo.C().foo()
           |                               ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        1 + from bar import D
        2 |
        3 | import foo
        4 |
          - a = foo.C().foo()
        5 + a: foo.B[foo.A[D[int, list[str | foo.A[foo.B[int]]]]]] = foo.C().foo()
          |
        ");
    }

    #[test]
    fn test_auto_import_with_update_import_from_statement() {
        let mut test = inlay_hint_test(
            "
            from foo import C

            a = C().foo()
            ",
        );

        test.with_extra_file(
            "foo.py",
            "
            import bar

            class A[T]: ...

            class B[T]: ...

            class C:
                def foo(self) -> B[A[bar.D[int, list[str | A[B[int]]]]]]:
                    raise NotImplementedError
                    ",
        );

        test.with_extra_file(
            "bar.py",
            "
            class D[T, U]: ...
            ",
        );

        assert_snapshot!(test.inlay_hints(), @"

        from foo import C

        a[: B[A[D[int, list[str | A[B[int]]]]]]] = C().foo()

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> foo.py:6:19
          |
        6 |             class B[T]: ...
          |                   ^
        info: Source
         --> main2.py:4:5
          |
        4 | a[: B[A[D[int, list[str | A[B[int]]]]]]] = C().foo()
          |     ^

        info[inlay-hint-location]: Inlay Hint Target
         --> foo.py:4:19
          |
        4 |             class A[T]: ...
          |                   ^
        info: Source
         --> main2.py:4:7
          |
        4 | a[: B[A[D[int, list[str | A[B[int]]]]]]] = C().foo()
          |       ^

        info[inlay-hint-location]: Inlay Hint Target
         --> bar.py:2:19
          |
        2 |             class D[T, U]: ...
          |                   ^
        info: Source
         --> main2.py:4:9
          |
        4 | a[: B[A[D[int, list[str | A[B[int]]]]]]] = C().foo()
          |         ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:11
           |
        LL | a[: B[A[D[int, list[str | A[B[int]]]]]]] = C().foo()
           |           ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:16
           |
        LL | a[: B[A[D[int, list[str | A[B[int]]]]]]] = C().foo()
           |                ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:21
           |
        LL | a[: B[A[D[int, list[str | A[B[int]]]]]]] = C().foo()
           |                     ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> foo.py:4:19
          |
        4 |             class A[T]: ...
          |                   ^
        info: Source
         --> main2.py:4:27
          |
        4 | a[: B[A[D[int, list[str | A[B[int]]]]]]] = C().foo()
          |                           ^

        info[inlay-hint-location]: Inlay Hint Target
         --> foo.py:6:19
          |
        6 |             class B[T]: ...
          |                   ^
        info: Source
         --> main2.py:4:29
          |
        4 | a[: B[A[D[int, list[str | A[B[int]]]]]]] = C().foo()
          |                             ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:31
           |
        LL | a[: B[A[D[int, list[str | A[B[int]]]]]]] = C().foo()
           |                               ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        1 + from bar import D
        2 |
          - from foo import C
        3 + from foo import C, B, A
        4 |
          - a = C().foo()
        5 + a: B[A[D[int, list[str | A[B[int]]]]]] = C().foo()
          |
        ");
    }

    #[test]
    fn test_auto_import_symbol_imported_from_different_path() {
        let mut test = inlay_hint_test(
            "
            from foo import D

            class Baz: ...

            a = D(Baz)
            ",
        );

        test.with_extra_file(
            "foo/__init__.py",
            "
            from foo.bar import D
                    ",
        );

        test.with_extra_file(
            "foo/bar.py",
            "
            class D[T]:
                def __init__(self, x: type[T]):
                    pass
            ",
        );

        assert_snapshot!(test.inlay_hints(), @"

        from foo import D

        class Baz: ...

        a[: D[Baz]] = D[[Baz]]([x=]Baz)

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> foo/bar.py:2:19
          |
        2 |             class D[T]:
          |                   ^
        info: Source
         --> main2.py:6:5
          |
        6 | a[: D[Baz]] = D[[Baz]]([x=]Baz)
          |     ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:4:7
          |
        4 | class Baz: ...
          |       ^^^
        info: Source
         --> main2.py:6:7
          |
        6 | a[: D[Baz]] = D[[Baz]]([x=]Baz)
          |       ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:4:7
          |
        4 | class Baz: ...
          |       ^^^
        info: Source
         --> main2.py:6:18
          |
        6 | a[: D[Baz]] = D[[Baz]]([x=]Baz)
          |                  ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> foo/bar.py:3:36
          |
        3 |                 def __init__(self, x: type[T]):
          |                                    ^
        info: Source
         --> main2.py:6:25
          |
        6 | a[: D[Baz]] = D[[Baz]]([x=]Baz)
          |                         ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        5 |
          - a = D(Baz)
        6 + a: D[Baz] = D(x=Baz)
          |
        ");
    }

    #[test]
    fn test_auto_import_typing_literal() {
        let mut test = inlay_hint_test(
            r#"
            from typing import Any

            def foo(x: Any):
                a = getattr(x, 'foo', "some")
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        from typing import Any

        def foo(x: Any):
            a[: Any | Literal["some"]] = getattr[[Literal["some"]]](x, 'foo', "some")

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:7
           |
        LL | class Any:
           |       ^^^
        info: Source
          --> main2.py:LL:9
           |
        LL |     a[: Any | Literal["some"]] = getattr[[Literal["some"]]](x, 'foo', "some")
           |         ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:15
           |
        LL |     a[: Any | Literal["some"]] = getattr[[Literal["some"]]](x, 'foo', "some")
           |               ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:23
           |
        LL |     a[: Any | Literal["some"]] = getattr[[Literal["some"]]](x, 'foo', "some")
           |                       ^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:43
           |
        LL |     a[: Any | Literal["some"]] = getattr[[Literal["some"]]](x, 'foo', "some")
           |                                           ^^^^^^^^^^^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        1 |
          - from typing import Any
        2 + from typing import Any, Literal
        3 |
        4 | def foo(x: Any):
          -     a = getattr(x, 'foo', "some")
        5 +     a: Any | Literal["some"] = getattr(x, 'foo', "some")
          |
        "#);
    }

    #[test]
    fn test_auto_import_other_symbols() {
        let mut test = inlay_hint_test(
            r#"
            from foo import foo

            a = foo()
            "#,
        );

        test.with_extra_file(
            "foo.py",
            r#"
        from typing import TypeVar, Any

        def foo() -> dict[TypeVar, Any] | None: ...
        "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        from foo import foo

        a[: dict[TypeVar, Any] | None] = foo()

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class dict[in out Key: Hashable, in out Value](MutableMapping[Key, Value]):
           |       ^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | a[: dict[TypeVar, Any] | None] = foo()
           |     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:13
           |
        LL | final class TypeVar:
           |             ^^^^^^^
        info: Source
          --> main2.py:LL:10
           |
        LL | a[: dict[TypeVar, Any] | None] = foo()
           |          ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:7
           |
        LL | class Any:
           |       ^^^
        info: Source
          --> main2.py:LL:19
           |
        LL | a[: dict[TypeVar, Any] | None] = foo()
           |                   ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/types.byi:LL:13
           |
        LL | final class NoneType:
           |             ^^^^^^^^
        info: Source
          --> main2.py:LL:26
           |
        LL | a[: dict[TypeVar, Any] | None] = foo()
           |                          ^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        1 + from typing import TypeVar
        2 + from typing import Any
        3 |
        4 | from foo import foo
        5 |
          - a = foo()
        6 + a: dict[TypeVar, Any] | None = foo()
          |
        ");
    }

    /// Tests that if we have an inlay hint containing two symbols with the same name
    /// from unimported modules, then we add two `import <module>` statements, and
    /// qualify both symbols (<module1.<symbol1>, <module2.<symbol1>).
    #[test]
    fn test_auto_import_same_name_different_modules_both_qualified() {
        let mut test = inlay_hint_test(
            r#"
            from foo import foo

            a = foo()
            "#,
        );

        test.with_extra_file(
            "foo.py",
            r#"
        import bar
        import baz

        def foo() -> bar.A | baz.A:
            return bar.A()
        "#,
        );

        test.with_extra_file(
            "bar.py",
            r#"
            class A: ...
        "#,
        );

        test.with_extra_file(
            "baz.py",
            r#"
            class A: ...
        "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        from foo import foo

        a[: bar.A | baz.A] = foo()

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> bar.py:2:19
          |
        2 |             class A: ...
          |                   ^
        info: Source
         --> main2.py:4:5
          |
        4 | a[: bar.A | baz.A] = foo()
          |     ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> baz.py:2:19
          |
        2 |             class A: ...
          |                   ^
        info: Source
         --> main2.py:4:13
          |
        4 | a[: bar.A | baz.A] = foo()
          |             ^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        1 + import bar
        2 + import baz
        3 |
        4 | from foo import foo
        5 |
          - a = foo()
        6 + a: bar.A | baz.A = foo()
          |
        ");
    }

    /// Tests that if we have an inlay hint containing two symbols with the same name
    /// from two modules, one which is imported already via a "import from" statement,
    /// then we still add two `import <module>` statements.
    ///
    /// We also show here that we don't add repeated import statements.
    #[test]
    fn test_auto_import_same_name_different_modules_one_qualified() {
        let mut test = inlay_hint_test(
            r#"
               from foo import foo
               from bar import B

               a = foo()
               "#,
        );

        test.with_extra_file(
            "foo.py",
            r#"
           import bar
           import baz

           def foo() -> bar.A | baz.A | list[bar.A | baz.A]:
               return bar.A()
           "#,
        );

        test.with_extra_file(
            "bar.py",
            r#"
               class A: ...
               class B: ...
           "#,
        );

        test.with_extra_file(
            "baz.py",
            r#"
               class A: ...
           "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        from foo import foo
        from bar import B

        a[: bar.A | baz.A | list[bar.A | baz.A]] = foo()

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> bar.py:2:22
          |
        2 |                class A: ...
          |                      ^
        info: Source
         --> main2.py:5:5
          |
        5 | a[: bar.A | baz.A | list[bar.A | baz.A]] = foo()
          |     ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> baz.py:2:22
          |
        2 |                class A: ...
          |                      ^
        info: Source
         --> main2.py:5:13
          |
        5 | a[: bar.A | baz.A | list[bar.A | baz.A]] = foo()
          |             ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:21
           |
        LL | a[: bar.A | baz.A | list[bar.A | baz.A]] = foo()
           |                     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> bar.py:2:22
          |
        2 |                class A: ...
          |                      ^
        info: Source
         --> main2.py:5:26
          |
        5 | a[: bar.A | baz.A | list[bar.A | baz.A]] = foo()
          |                          ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> baz.py:2:22
          |
        2 |                class A: ...
          |                      ^
        info: Source
         --> main2.py:5:34
          |
        5 | a[: bar.A | baz.A | list[bar.A | baz.A]] = foo()
          |                                  ^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        1 + import bar
        2 + import baz
        3 |
        4 | from foo import foo
        5 | from bar import B
        6 |
          - a = foo()
        7 + a: bar.A | baz.A | list[bar.A | baz.A] = foo()
          |
        ");
    }

    /// Tests that if we have an inlay hint containing a symbol that is referenced
    /// in another module, that we qualify the inlay hint symbol with the module name,
    /// so we don't accidentally reference the in scope symbol.
    #[test]
    fn test_auto_import_symbol_in_scope_same_name() {
        let mut test = inlay_hint_test(
            r#"
                from dataclasses import dataclass
                import foo

                class A: ...

                @dataclass
                class B[T]:
                    x: T

                b = B(foo.A())
               "#,
        );

        test.with_extra_file(
            "foo.py",
            r#"
            class A: ...
           "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        from dataclasses import dataclass
        import foo

        class A: ...

        @dataclass
        class B[T]:
            x: T

        b[: B[A]] = B[[A]]([x=]foo.A())

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:8:7
          |
        8 | class B[T]:
          |       ^
        info: Source
          --> main2.py:11:5
           |
        11 | b[: B[A]] = B[[A]]([x=]foo.A())
           |     ^

        info[inlay-hint-location]: Inlay Hint Target
         --> foo.py:2:19
          |
        2 |             class A: ...
          |                   ^
        info: Source
          --> main2.py:11:7
           |
        11 | b[: B[A]] = B[[A]]([x=]foo.A())
           |       ^

        info[inlay-hint-location]: Inlay Hint Target
         --> foo.py:2:19
          |
        2 |             class A: ...
          |                   ^
        info: Source
          --> main2.py:11:16
           |
        11 | b[: B[A]] = B[[A]]([x=]foo.A())
           |                ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:9:5
          |
        9 |     x: T
          |     ^
        info: Source
          --> main2.py:11:21
           |
        11 | b[: B[A]] = B[[A]]([x=]foo.A())
           |                     ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        10 |
           - b = B(foo.A())
        11 + b: B[foo.A] = B(x=foo.A())
           |
        ");
    }

    #[test]
    fn test_auto_import_enum_member() {
        let mut test = inlay_hint_test(
            r#"
            from test import Color

            x = Color.RED
            "#,
        );

        test.with_extra_file(
            "test.py",
            r#"
            from enum import Enum

            class Color(Enum):
                RED = 1
                BLUE = 2
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        from test import Color

        x[: Literal[Color.RED]] = Color.RED

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | x[: Literal[Color.RED]] = Color.RED
           |     ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> test.py:4:19
          |
        4 |             class Color(Enum):
          |                   ^^^^^
        info: Source
         --> main2.py:4:13
          |
        4 | x[: Literal[Color.RED]] = Color.RED
          |             ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> test.py:5:17
          |
        5 |                 RED = 1
          |                 ^^^
        info: Source
         --> main2.py:4:19
          |
        4 | x[: Literal[Color.RED]] = Color.RED
          |                   ^^^
        ");
    }

    /// Regression test for astral-sh/ty#3313: applying the inlay hint on `y`
    /// previously added `Inner` to `from module import Outer`, but `Inner` is
    /// a nested class inside `Outer`, not a top-level symbol of `module`.
    #[test]
    fn test_auto_import_nested_class() {
        let mut test = inlay_hint_test(
            r#"
            from module import Outer


            def wrap[T](x: T) -> list[T]:
                return [x]

            y = wrap(Outer.Inner())
            "#,
        );

        test.with_extra_file(
            "module.py",
            r#"
            class Outer:
                class Inner: ...
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        from module import Outer


        def wrap[T](x: T) -> list[T]:
            return [x]

        y[: list[Inner]] = wrap[[Inner]]([x=]Outer.Inner())

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | y[: list[Inner]] = wrap[[Inner]]([x=]Outer.Inner())
           |     ^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> module.py:3:23
          |
        3 |                 class Inner: ...
          |                       ^^^^^
        info: Source
         --> main2.py:8:10
          |
        8 | y[: list[Inner]] = wrap[[Inner]]([x=]Outer.Inner())
          |          ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> module.py:3:23
          |
        3 |                 class Inner: ...
          |                       ^^^^^
        info: Source
         --> main2.py:8:26
          |
        8 | y[: list[Inner]] = wrap[[Inner]]([x=]Outer.Inner())
          |                          ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:5:13
          |
        5 | def wrap[T](x: T) -> list[T]:
          |             ^
        info: Source
         --> main2.py:8:35
          |
        8 | y[: list[Inner]] = wrap[[Inner]]([x=]Outer.Inner())
          |                                   ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        7 |
          - y = wrap(Outer.Inner())
        8 + y = wrap(x=Outer.Inner())
          |
        ");
    }

    #[test]
    fn test_auto_import_enum_member_unimported_class() {
        let mut test = inlay_hint_test(
            r#"
            import test

            x = test.Color.RED
            "#,
        );

        test.with_extra_file(
            "test.py",
            r#"
            from enum import Enum

            class Color(Enum):
                RED = 1
                BLUE = 2
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        import test

        x[: Literal[Color.RED]] = test.Color.RED

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:5
           |
        LL | x[: Literal[Color.RED]] = test.Color.RED
           |     ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> test.py:4:19
          |
        4 |             class Color(Enum):
          |                   ^^^^^
        info: Source
         --> main2.py:4:13
          |
        4 | x[: Literal[Color.RED]] = test.Color.RED
          |             ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> test.py:5:17
          |
        5 |                 RED = 1
          |                 ^^^
        info: Source
         --> main2.py:4:19
          |
        4 | x[: Literal[Color.RED]] = test.Color.RED
          |                   ^^^
        ");
    }

    #[test]
    fn test_auto_import_method_returning_nested_class() {
        let mut test = inlay_hint_test(
            r#"
            from module import Outer

            x = Outer().make()
            "#,
        );

        test.with_extra_file(
            "module.py",
            r#"
            class Outer:
                class Inner: ...

                def make(self) -> "Outer.Inner":
                    return Outer.Inner()
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        from module import Outer

        x[: Inner] = Outer().make()

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> module.py:3:23
          |
        3 |                 class Inner: ...
          |                       ^^^^^
        info: Source
         --> main2.py:4:5
          |
        4 | x[: Inner] = Outer().make()
          |     ^^^^^
        ");
    }

    #[test]
    fn test_auto_import_same_file_method_returning_nested_class() {
        let mut test = inlay_hint_test(
            r#"
            class Outer:
                class Inner: ...

                def make(self) -> "Outer.Inner":
                    return Outer.Inner()

            x = Outer().make()
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        class Outer:
            class Inner: ...

            def make(self) -> "Outer.Inner":
                return Outer.Inner()

        x[: Inner] = Outer().make()

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:3:11
          |
        3 |     class Inner: ...
          |           ^^^^^
        info: Source
         --> main2.py:8:5
          |
        8 | x[: Inner] = Outer().make()
          |     ^^^^^
        "#);
    }

    struct InlayHintLocationDiagnostic {
        source: FileRange,
        target: FileRange,
    }

    impl InlayHintLocationDiagnostic {
        fn new(source: FileRange, target: &NavigationTarget) -> Self {
            Self {
                source,
                target: FileRange::new(target.file(), target.focus_range()),
            }
        }
    }

    impl IntoDiagnostic for InlayHintLocationDiagnostic {
        fn into_diagnostic(self) -> Diagnostic {
            let mut source = SubDiagnostic::new(SubDiagnosticSeverity::Info, "Source");

            source.annotate(Annotation::primary(
                Span::from(self.source.file()).with_range(self.source.range()),
            ));

            let mut main = Diagnostic::new(
                DiagnosticId::Lint(LintName::of("inlay-hint-location")),
                Severity::Info,
                "Inlay Hint Target".to_string(),
            );

            main.annotate(Annotation::primary(
                Span::from(self.target.file()).with_range(self.target.range()),
            ));

            main.sub(source);

            main
        }
    }

    struct InlayHintEditDiagnostic<'a> {
        file: File,
        first_edit: &'a InlayHintTextEdit,
        rest: &'a [InlayHintTextEdit],
    }

    impl<'a> InlayHintEditDiagnostic<'a> {
        fn new(
            file: File,
            first_edit: &'a InlayHintTextEdit,
            rest: &'a [InlayHintTextEdit],
        ) -> Self {
            Self {
                file,
                first_edit,
                rest,
            }
        }
    }

    impl IntoDiagnostic for InlayHintEditDiagnostic<'_> {
        fn into_diagnostic(self) -> Diagnostic {
            let mut main = Diagnostic::new(
                DiagnosticId::Lint(LintName::of("inlay-hint-edit")),
                Severity::Info,
                "Inlay hint edits".to_string(),
            );

            let mut annotation = Annotation::primary(Span::from(self.file));
            annotation.hide_snippet(true);
            main.annotate(annotation);

            // These fixes aren't actually safe but using `safe` has the benefit over unsafe
            // that it doesn't render a noisy "This is an unsafe fix" note
            let fix = Fix::safe_edits(
                self.first_edit.to_fix_edit(),
                self.rest.iter().map(InlayHintTextEdit::to_fix_edit),
            );

            main.set_fix(fix);

            main
        }
    }

    #[test]
    fn basedpython_inferred_raises() {
        let mut test = basedpython_inlay_hint_test(
            "
            def leaf():
                raise TypeError

            def caller():
                leaf()

            def caught():
                try:
                    leaf()
                except TypeError:
                    pass

            def annotated() -> int:
                raise ValueError

            def declared() raises TypeError:
                raise TypeError
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_raises: true,
            ..InlayHintSettings::none()
        }));
    }

    #[test]
    fn basedpython_inferred_variance() {
        let mut test = basedpython_inlay_hint_test(
            "
            class Source[T]:
                def get(self) -> T: ...

            class Sink[T]:
                def put(self, value: T) -> None: ...

            class Both[T]:
                value: T

            class Declared[out T]:
                def get(self) -> T: ...

            class Plain:
                pass

            type Alias[T] = list[T]
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_variance: true,
            ..InlayHintSettings::none()
        }));
    }

    /// a pack declares no variance of its own — there is no `out *Ts` to write —
    /// so the variance it is inferred to have is always worth showing
    #[test]
    fn basedpython_inferred_variance_of_a_pack() {
        let mut test = basedpython_inlay_hint_test(
            "
            class Source[*Ts]:
                def get(self) -> (*Ts,): ...

            class Sink[**Kwargs]:
                def put(self) -> (**Kwargs) -> None: ...
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_variance: true,
            ..InlayHintSettings::none()
        }));
    }

    /// a declaration that names no type is where the type would be written, so it
    /// is hinted like the assignment it is. a declaration that already names one
    /// has nothing to add
    #[test]
    fn basedpython_declaration_variable_types() {
        let mut test = basedpython_inlay_hint_test(
            "
            def foo() -> int:
                return 1

            let a = foo()
            var b = foo()
            context c = foo()
            final d = foo()
            let e: int = foo()
            var f: int = foo()
            let g
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            variable_types: true,
            ..InlayHintSettings::none()
        }));
    }

    #[test]
    fn basedpython_inferred_reification() {
        let mut test = basedpython_inlay_hint_test(
            "
            def value_use[T]():
                print(T)

            def pack_use[*Ts, **Kwargs]():
                print(Ts, Kwargs)

            def annotation_only[T](t: T) -> T:
                return t

            def declared[reified T]():
                pass

            def half_declared[reified T, U]():
                print(U)

            class C:
                def method[T](self):
                    print(T)
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_reification: true,
            ..InlayHintSettings::none()
        }));
    }

    #[test]
    fn basedpython_inferred_override() {
        let mut test = basedpython_inlay_hint_test(
            "
            class A:
                def f(self) -> None: ...
                def g(self) -> None: ...
                def __init__(self) -> None: ...

            class B(A):
                def f(self) -> None: ...
                override def g(self) -> None: ...
                def h(self) -> None: ...
                def __init__(self) -> None: ...
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_override: true,
            ..InlayHintSettings::none()
        }));
    }

    #[test]
    fn basedpython_implicit_parameters() {
        let mut test = basedpython_inlay_hint_test(
            "
            def apply(fn: (int) -> None) -> None:
                fn(1)

            def against(fn: str.() -> None) -> None:
                'a'.fn()

            def against_with(fn: str.(int) -> None) -> None:
                'a'.fn(1)

            class C:
                init(a: int)

            class D:
                init()

            apply:
                print(it)

            against:
                print(upper())

            against_with:
                print(upper(), it)
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            implicit_parameters: true,
            ..InlayHintSettings::none()
        }));
    }

    /// A callback that passes no argument gives the block no `it` to bind, so
    /// there is no implicit parameter to hint — hinting one would name something
    /// the body cannot resolve.
    #[test]
    fn basedpython_implicit_parameters_without_an_argument() {
        let mut test = basedpython_inlay_hint_test(
            "
            def apply(fn: () -> None) -> None:
                fn()

            def against(fn: str.() -> None) -> None:
                'a'.fn()

            apply:
                print('hi')

            against:
                print(upper())
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            implicit_parameters: true,
            ..InlayHintSettings::none()
        }));
    }

    /// A block standing as a statement's value binds the same parameters, and
    /// the variable it binds is hinted with the call's type, not the callee's.
    #[test]
    fn basedpython_implicit_parameters_as_a_value() {
        let mut test = basedpython_inlay_hint_test(
            "
            def apply(fn: (int) -> None) -> str:
                fn(1)
                return 'done'

            result = apply:
                print(it)
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            implicit_parameters: true,
            variable_types: true,
            ..InlayHintSettings::none()
        }));
    }

    /// The `self` an `init(...)` binds is hinted under its own setting, so it
    /// can be turned off without losing a trailing lambda's `it`. A property
    /// accessor's synthesized header is not hinted at all.
    #[test]
    fn basedpython_implicit_self() {
        let mut test = basedpython_inlay_hint_test(
            "
            def apply(fn: (int) -> None) -> None:
                fn(1)

            class C:
                init(a: int)

            class D:
                init()

            class E:
                var x: int = 0
                    get() = field

            apply:
                print(it)
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            implicit_self: true,
            ..InlayHintSettings::none()
        }));
    }

    /// Types in a `.by` file are spelled the way that file is written, not in
    /// typing-spec syntax — a hint is read as source.
    #[test]
    fn basedpython_type_display() {
        let mut test = basedpython_inlay_hint_test(
            "
            def identity[T](x: T) -> T:
                return x

            a = identity(1)
            b = (1, 'two')
            reveal_type(a)
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            variable_types: true,
            call_type_arguments: true,
            revealed_types: true,
            ..InlayHintSettings::none()
        }));
    }

    #[test]
    fn basedpython_implicit_context_arguments() {
        let mut test = basedpython_inlay_hint_test(
            "
            def f(context a: int) -> None: ...
            def g(x: str, context a: int, context c: str) -> None: ...

            context b = 1
            context d = 'x'

            f()
            f(a=2)
            g('y')
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            implicit_arguments: true,
            ..InlayHintSettings::none()
        }));
    }

    /// a trailing lambda block's implicit names fill `context` parameters too.
    /// nothing is written for them, so unlike a `context` declaration they have
    /// no definition for the hint to navigate to.
    #[test]
    fn basedpython_implicit_context_arguments_from_a_block() {
        let mut test = basedpython_inlay_hint_test(
            "
            def f(context a: int) -> None: ...
            def each(fn: (int) -> None) -> None: ...
            def against(fn: int.() -> None) -> None: ...

            each:
                f()

            against:
                f()
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            implicit_arguments: true,
            ..InlayHintSettings::none()
        }));
    }

    #[test]
    fn basedpython_string_tag_argument_is_not_hinted() {
        let mut test = basedpython_inlay_hint_test(
            "
            def sql(query: str) -> int:
                return 0

            a = sql\"select\"
            b = sql(\"select\")
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            call_argument_names: true,
            ..InlayHintSettings::none()
        }));
    }

    /// Every cast form parses as a synthetic `cast(<type>, <value>)` call, but its
    /// operands are the operator's own surface syntax — the reader passed nothing
    /// by position, so naming the parameters would be noise.
    #[test]
    fn basedpython_cast_operands_are_not_hinted() {
        let mut test = basedpython_inlay_hint_test(
            "
            def f(a: object, b: int):
                x = b cast object
                y = a cast! int
                z = a cast? int
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            call_argument_names: true,
            ..InlayHintSettings::none()
        }));
    }

    /// basedpython infers a constructor call as `final A`, but the hint offers the
    /// type the declaration would have — writing the modifier down would be a
    /// stricter declaration than the code asked for.
    #[test]
    fn basedpython_constructor_hint_drops_the_final_modifier() {
        let mut test = basedpython_inlay_hint_test(
            "
            class Wrapper[Element]:
                init(element: Element)

            def f():
                a = Wrapper(1)
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            variable_types: true,
            ..InlayHintSettings::none()
        }));
    }

    /// basedpython names the type parameter each type argument fills wherever a
    /// specialization is rendered, matching the keyword subscript it can be
    /// written as.
    #[test]
    fn basedpython_named_type_arguments() {
        let mut test = basedpython_inlay_hint_test(
            "
            class Pair[Key, Value]:
                init(key: Key, value: Value)

            class One[Element]:
                init(element: Element)

            a = Pair(1, 'x')
            b = One(1)
            c: dict[str, int] = {}
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            variable_types: true,
            call_type_arguments: true,
            type_argument_names: true,
            ..InlayHintSettings::none()
        }));
    }

    /// A `some` parameter opens an anonymous hole, which is not a position a call
    /// site can supply, so there is no specialization to offer for it.
    #[test]
    fn basedpython_some_holes_are_not_hinted() {
        let mut test = basedpython_inlay_hint_test(
            "
            def echo(s: some str) -> s:
                return s

            def pair[Element](s: some str, e: Element) -> Element:
                return e

            a = echo('lit')
            b = pair('lit', 1)
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            call_type_arguments: true,
            type_argument_names: true,
            ..InlayHintSettings::none()
        }));
    }

    /// The hole an unannotated parameter opens under `sound-types` is not a position a
    /// call site can supply either, so it is left out the same way a written `some` is.
    #[test]
    fn basedpython_inferred_holes_are_not_hinted() {
        let mut test = sound_types_inlay_hint_test(
            "
            def echo(s):
                return s

            def pair[Element](s, e: Element) -> Element:
                return e

            a = echo('lit')
            b = pair('lit', 1)
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            call_type_arguments: true,
            type_argument_names: true,
            ..InlayHintSettings::none()
        }));
    }

    /// A `.py` file has no keyword subscript to spell a named type argument, so
    /// a rendered specialization stays positional there.
    #[test]
    fn type_arguments_are_not_named_outside_basedpython() {
        let mut test = inlay_hint_test(
            "
            class Pair[Key, Value]:
                def __init__(self, key: Key, value: Value) -> None: ...

            a = Pair(1, 'x')
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            variable_types: true,
            call_type_arguments: true,
            type_argument_names: true,
            ..InlayHintSettings::none()
        }));
    }

    #[test]
    fn call_type_arguments() {
        let mut test = inlay_hint_test(
            "
            def identity[T](x: T) -> T:
                return x

            def pair[T, U](a: T, b: U) -> tuple[T, U]:
                return (a, b)

            def plain(x: int) -> int:
                return x

            class Box[T]:
                def __init__(self, value: T) -> None: ...

            identity(1)
            pair('a', 2)
            plain(1)
            Box(1)
            Box[str]('a')
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            call_type_arguments: true,
            ..InlayHintSettings::none()
        }));
    }

    #[test]
    fn basedpython_declared_variance_beside_an_inferred_parameter() {
        // `B` is used only by the constructor, so it is bivariant and
        // constrains nothing — its argument still reaches the specialization
        let mut test = basedpython_inlay_hint_test(
            "
            class A[out A, B]:
                init(a: A, b: B)

            a = A(1, 2)
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            variable_types: true,
            call_type_arguments: true,
            ..InlayHintSettings::none()
        }));
    }

    #[test]
    fn type_argument_names() {
        let mut test = inlay_hint_test(
            "
            class Cache[Key, Value]:
                pass

            def f(
                a: dict[str, int],
                b: list[int],
                c: Cache[str, int],
                d: tuple[int, str],
            ) -> None: ...
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            type_argument_names: true,
            ..InlayHintSettings::none()
        }));
    }

    #[test]
    fn basedpython_type_argument_names() {
        let mut test = basedpython_inlay_hint_test(
            "
            class Cache[Key, Value]: ...

            type Alias = Cache[str, int]

            def f(
                a: Cache[Key=str, Value=int],
                b: Cache[str, Value=int],
            ) -> None: ...
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            type_argument_names: true,
            ..InlayHintSettings::none()
        }));
    }

    #[test]
    fn numeric_promotions() {
        let mut test = inlay_hint_test(
            "
            def f(x: float, y: complex, z: int) -> float:
                return x

            a: float = 1.0
            b: list[float] = []
            c = float(1)
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            numeric_promotions: true,
            ..InlayHintSettings::none()
        }));
    }

    /// An arm the union already writes is not one the promotion adds — `float |
    /// int` names two arms whichever way it is read — so only the arms missing
    /// from the union are hinted. A `float` nested inside an operand sits in a
    /// union of its own and keeps all of its arms.
    #[test]
    fn numeric_promotions_already_written_in_the_union() {
        let mut test = inlay_hint_test(
            "
            from typing import Union

            def f(
                a: float | int,
                b: int | float,
                c: complex | int,
                d: complex | float,
                e: float | str,
                f: Union[float, int],
                g: Union[float, str],
                h: list[float] | int,
                i: str | float | int,
            ) -> None: ...
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            numeric_promotions: true,
            ..InlayHintSettings::none()
        }));
    }

    #[test]
    fn basedpython_numeric_promotions_are_not_hinted() {
        let mut test = basedpython_inlay_hint_test(
            "
            def f(x: float, y: complex) -> None: ...
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            numeric_promotions: true,
            ..InlayHintSettings::none()
        }));
    }

    /// A place declared wider than the value that reaches the call shows both, so the narrowing
    /// is visible. A place with nothing declared, or one read at its full declared type, shows
    /// only what the call revealed.
    #[test]
    fn revealed_types() {
        let mut test = inlay_hint_test(
            "
            reveal_type(1)

            x = 'a'
            reveal_type(x)  # a trailing comment

            def f(y: int) -> None:
                reveal_type(y)

            a: int = 1
            reveal_type(a)

            def g(z: int | str) -> None:
                if isinstance(z, int):
                    reveal_type(z)
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            revealed_types: true,
            ..InlayHintSettings::none()
        }));
    }

    #[test]
    fn lambda_parameter_types() {
        let mut test = inlay_hint_test(
            "
            def apply(fn: Callable[[int], str]) -> None: ...

            from typing import Callable

            apply(lambda x: str(x))
            f = lambda y: y
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            lambda_parameter_types: true,
            ..InlayHintSettings::none()
        }));
    }

    /// An unannotated method takes its parameter types from the method it
    /// overrides, so the hint says what the override left out. A method that
    /// overrides nothing has only an anonymous hole to offer, which says no more
    /// than the missing annotation did.
    #[test]
    fn basedpython_inherited_parameter_types() {
        let mut test = basedpython_inlay_hint_test(
            "
            class A:
                def f(self, a: int, b: str = 'x') -> None: ...

            class B(A):
                def f(self, a, b='y') -> None: ...

            class C:
                def f(self, a) -> None: ...
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inherited_parameter_types: true,
            ..InlayHintSettings::none()
        }));
    }

    /// An overload implementation takes its parameter types from the overloads
    /// it implements, the same way an override takes them from its base.
    #[test]
    fn basedpython_inherited_parameter_types_from_overloads() {
        let mut test = basedpython_inlay_hint_test(
            "
            from typing import overload

            @overload
            def f(a: int) -> int: ...
            @overload
            def f(a: str) -> str: ...
            def f(a): ...
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inherited_parameter_types: true,
            ..InlayHintSettings::none()
        }));
    }

    /// The return type of a `def` that leaves its annotation out is recovered
    /// from the body, and shown where the annotation would go — ahead of a
    /// `raises` clause, which is written after it.
    ///
    /// `None` is what such a `def` already means, so it is not worth a hint.
    #[test]
    fn basedpython_inferred_return_types() {
        let mut test = basedpython_inlay_hint_test(
            "
            def f():
                return 1

            def g():
                print('hi')

            def h(a: int):
                if a:
                    return 'x'
                return None

            def raiser():
                raise TypeError
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_return_types: true,
            inferred_raises: true,
            ..InlayHintSettings::none()
        }));
    }

    /// A written return annotation is the source's own answer, and an
    /// `init(...)` is given its `-> None` by the parser, so neither is hinted.
    #[test]
    fn basedpython_declared_return_types_are_not_hinted() {
        let mut test = basedpython_inlay_hint_test(
            "
            def f() -> int:
                return 1

            def guard(a: object) -> asserts a:
                assert a

            class C:
                init(a: int)
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_return_types: true,
            ..InlayHintSettings::none()
        }));
    }

    impl InlayHintTextEdit {
        fn to_fix_edit(&self) -> Edit {
            if self.range.is_empty() {
                Edit::insertion(self.new_text.clone(), self.range.start())
            } else {
                Edit::range_replacement(self.new_text.clone(), self.range)
            }
        }
    }
}
