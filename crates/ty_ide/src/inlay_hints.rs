use std::{fmt, vec};
use ty_python_semantic::ProgramEnvironment;

use itertools::{Either, Itertools};
use rustc_hash::FxHashMap;
use strum_macros::EnumIter;

use crate::import_order;

mod stub;
use crate::{Db, HasNavigationTargets, NavigationTarget};
use ruff_db::parsed::parsed_module;
use ruff_db::source::{line_index, source_text};
use ruff_python_ast::helpers::is_compound_statement;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::source_order::{self, SourceOrderVisitor, TraversalSignal};
use ruff_python_ast::{
    self as ast, AnyNodeRef, ArgOrKeyword, Expr, ExprUnaryOp, PySourceType, Stmt, UnaryOp,
};
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange, TextSize};
pub(crate) use stub::stub_member_fixes;
use ty_module_resolver::file_to_module;
use ty_python_core::ProgramFile;
use ty_python_core::scope::FileScopeId;
use ty_python_semantic::importer::{ImportAction, ImportRequest, Importer, MembersInScope};
use ty_python_semantic::reified::{
    inferred_reified_class_type_param_names, inferred_reified_type_param_names,
};
use ty_python_semantic::types::context_params::implicit_context_arguments;
use ty_python_semantic::types::ide_support::{
    BaseTypeArgument, InferredInvalidations, InferredReturnPredicate, InferredStateReads,
    InlayHintCallArgumentDetails, ParameterTypeSource, StateRead, WriteSite,
    hintable_parameter_type, implicit_enum_member_value, implicit_type_name_module,
    inferred_base_type_arguments, inferred_derived_dependencies, inferred_invalidations,
    inferred_override, inferred_parameter_bound, inferred_parameter_holes, inferred_property_type,
    inferred_raise_sites, inferred_raises, inferred_return_predicate, inferred_return_type,
    inferred_state_reads, inferred_type_param_variance, inherited_default_source,
    inherited_parameter_annotation, inherited_parameter_default, inherited_parameter_source,
    inlay_hint_call_argument_details, is_composable_function, is_inferred_parameter_hole,
    is_reveal_type_function, is_union_special_form, numeric_promotion, parameter_stability,
    trailing_lambda_implicit_parameters, type_parameter_names,
};
use ty_python_semantic::types::{
    BoundTypeVarInstance, DisplaySettings, Type, TypeDetail, TypeDisplayDetails,
};
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
    /// `None` for a hint drawn inline, at its position. a hint drawn as whole lines of its own,
    /// as a member only the stub declares is, says here whether they go above or below the line
    /// holding its position
    pub placement: Option<BlockPlacement>,
    /// markdown saying what the label does not: where the hint comes from, or why it is there.
    /// `None` where the label says all there is to say
    pub tooltip: Option<String>,
}

/// which side of the line holding a whole-line hint's position the hint's lines are drawn on
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockPlacement {
    Above,
    Below,
}

impl BlockPlacement {
    /// what a client knows the placement by
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Above => "above",
            Self::Below => "below",
        }
    }
}

impl InlayHint {
    fn variable_type<'db>(
        context: InlayHintImportContext<'_, 'db>,
        expr: &Expr,
        rhs: &Expr,
        ty: Type<'db>,
        allow_edits: bool,
        named_type_arguments: bool,
        type_params_in_scope: &[(BoundTypeVarInstance<'db>, Name)],
    ) -> Option<Self> {
        let position = expr.range().end();
        let details = type_display_details(
            context.db,
            context.file,
            ty,
            named_type_arguments,
            type_params_in_scope,
        );

        // Filter out repetitive hints like `x: T = T()`
        if call_matches_name(rhs, &details.label) {
            return None;
        }

        // each hint carries its own imports, because a client can apply one without the others
        let spelled = spell_type(
            context,
            &mut FxHashMap::default(),
            expr.into(),
            expr.range().start(),
            &details,
            type_params_in_scope,
        );

        let text_edits = if spelled.editable && allow_edits {
            let mut text_edits = vec![InlayHintTextEdit {
                range: TextRange::new(position, position),
                new_text: format!(": {}", spelled.edit_label),
            }];
            text_edits.extend(spelled.import_edits);
            text_edits
        } else {
            vec![]
        };

        let mut label_parts = vec![": ".into()];
        label_parts.extend(spelled.label_parts);

        Some(Self {
            position,
            kind: InlayHintKind::VariableType,
            label: InlayHintLabel { parts: label_parts },
            padding_left: false,
            padding_right: false,
            text_edits,
            placement: None,
            tooltip: None,
        })
    }

    /// the name of the parameter a positional argument is passed to, before it. the edit that
    /// writes it is the call's to give, see `name_positional_arguments`, and the tooltip is the
    /// parameter as its signature declares it
    fn call_argument_name(
        position: TextSize,
        name: &str,
        navigation_target: Option<NavigationTarget>,
        declaration: Option<&str>,
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
            placement: None,
            tooltip: declaration.map(|declaration| {
                crate::MarkupKind::Markdown
                    .fenced_code_block(declaration, "python")
                    .to_string()
            }),
        }
    }

    /// basedpython: the exception set inferred for a function with no `raises`
    /// clause, shown where the clause would be written, with the edit that writes
    /// it there and imports what it names.
    ///
    /// the tooltip lists where each exception escapes the body from.
    fn inferred_raises<'db>(
        context: InlayHintImportContext<'_, 'db>,
        function: &ast::StmtFunctionDef,
        position: TextSize,
        raised: Type<'db>,
        type_params_in_scope: &[(BoundTypeVarInstance<'db>, Name)],
        tooltip: Option<String>,
    ) -> Self {
        let mut imports = FxHashMap::default();
        let mut speller = HintSpeller::new(
            context,
            &mut imports,
            function.into(),
            function.start(),
            type_params_in_scope,
            &[],
        );
        speller.text("raises ");
        speller.ty(raised);
        let (parts, written) = speller.finish();

        Self {
            position,
            kind: InlayHintKind::Raises,
            label: InlayHintLabel { parts },
            padding_left: true,
            padding_right: false,
            // the label's padding is the client's, so the edit writes the space itself
            text_edits: insertion_edits(position, written, |text| format!(" {text}")),
            placement: None,
            tooltip,
        }
    }

    /// basedpython: the variance inferred for a type parameter that declares
    /// none, shown where the keyword would be written: before the name, after a
    /// `reified` the parameter writes.
    ///
    /// `writable` says the keyword can be written there: a variadic pack has no
    /// variance syntax, and a reified parameter may not declare one.
    fn inferred_variance(position: TextSize, variance: ast::Variance, writable: bool) -> Self {
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
            text_edits: if writable {
                vec![InlayHintTextEdit {
                    range: TextRange::empty(position),
                    new_text: format!("{keyword} "),
                }]
            } else {
                vec![]
            },
            placement: None,
            tooltip: None,
        }
    }

    /// basedpython: a type parameter reified by a value-position use in the
    /// body rather than by the keyword, shown where the keyword would go.
    ///
    /// `writable` says the keyword can be written there: a parameter that
    /// declares a variance cannot also be reified.
    fn inferred_reification(position: TextSize, writable: bool) -> Self {
        Self {
            position,
            kind: InlayHintKind::Reification,
            label: InlayHintLabel {
                parts: vec!["reified".into()],
            },
            padding_left: false,
            padding_right: true,
            text_edits: if writable {
                vec![InlayHintTextEdit {
                    range: TextRange::empty(position),
                    new_text: "reified ".to_string(),
                }]
            } else {
                vec![]
            },
            placement: None,
            tooltip: None,
        }
    }

    /// basedpython: a method that overrides a superclass member without saying
    /// so, shown where the `override` modifier would be written, with the edit
    /// that writes it, and naming the member it overrides as its tooltip.
    fn inferred_override(
        position: TextSize,
        superclass: Option<NavigationTarget>,
        overridden: &str,
    ) -> Self {
        Self {
            position,
            kind: InlayHintKind::Override,
            label: InlayHintLabel {
                parts: vec![InlayHintLabelPart::new("override").with_target(superclass)],
            },
            padding_left: false,
            padding_right: true,
            // modifiers are written in any order ahead of the `def`, so this is a
            // place for one whatever the definition already writes
            text_edits: vec![InlayHintTextEdit {
                range: TextRange::empty(position),
                new_text: "override ".to_string(),
            }],
            placement: None,
            tooltip: Some(format!("Overrides {}", inline_code(overridden))),
        }
    }

    /// basedpython-ui: the observables a function reads while composing, shown
    /// on its header after the return annotation and `raises` clause.
    fn inferred_reads(position: TextSize, reads: &InferredStateReads) -> Self {
        Self {
            position,
            kind: InlayHintKind::Reads,
            label: InlayHintLabel {
                parts: state_read_parts("reads ", &reads.reads, reads.opaque),
            },
            padding_left: true,
            padding_right: false,
            text_edits: vec![],
            placement: None,
            tooltip: None,
        }
    }

    /// basedpython-ui: a composable parameter the runtime cannot compare, shown
    /// where a modifier would be written before its name. A stable parameter
    /// is the ordinary case and gets no hint.
    fn unstable_parameter(position: TextSize) -> Self {
        Self {
            position,
            kind: InlayHintKind::Stability,
            label: InlayHintLabel {
                parts: vec!["unstable".into()],
            },
            padding_left: false,
            padding_right: true,
            text_edits: vec![],
            placement: None,
            tooltip: None,
        }
    }

    /// basedpython-ui: what a `derived(...)` / `remember(...)` computation
    /// depends on, shown at the end of the call's line.
    fn derived_dependencies(position: TextSize, reads: &InferredStateReads) -> Self {
        Self {
            position,
            kind: InlayHintKind::DerivedDeps,
            label: InlayHintLabel {
                parts: state_read_parts("depends on ", &reads.reads, reads.opaque),
            },
            padding_left: true,
            padding_right: false,
            text_edits: vec![],
            placement: None,
            tooltip: None,
        }
    }

    /// basedpython-ui: the composition scopes a state write made after
    /// composing invalidates — the composables and `derived` computations
    /// that read what is written — shown at the end of the statement, or of
    /// the lambda, that writes. An empty set is spelled `nothing`: a write
    /// nobody observes is worth seeing.
    fn invalidations(position: TextSize, invalidations: &InferredInvalidations) -> Self {
        let parts = if invalidations.scopes.is_empty() && !invalidations.opaque {
            vec!["invalidates nothing".into()]
        } else {
            state_read_parts("invalidates ", &invalidations.scopes, invalidations.opaque)
        };
        Self {
            position,
            kind: InlayHintKind::Invalidates,
            label: InlayHintLabel { parts },
            padding_left: true,
            padding_right: false,
            text_edits: vec![],
            placement: None,
            tooltip: None,
        }
    }

    /// The type arguments inferred for a generic call, shown between the callee
    /// and its argument list — where an explicit specialization would be written.
    ///
    /// `writable` says the file can specialize the callee there: a `.by` file can
    /// any generic callee, a `.py` file only a class, since a python function has
    /// no `__getitem__`.
    fn call_type_arguments<'db>(
        context: InlayHintImportContext<'_, 'db>,
        call: &ast::ExprCall,
        position: TextSize,
        arguments: &[(Name, Type<'db>)],
        named: bool,
        writable: bool,
        type_params_in_scope: &[(BoundTypeVarInstance<'db>, Name)],
    ) -> Self {
        // a lone type parameter has nothing to disambiguate, so naming it would
        // be noise rather than orientation
        let named = named && arguments.len() > 1;

        let among: Vec<_> = arguments.iter().map(|(_, argument)| *argument).collect();
        let mut imports = FxHashMap::default();
        let mut speller = HintSpeller::new(
            context,
            &mut imports,
            call.into(),
            call.start(),
            type_params_in_scope,
            &among,
        );
        speller.text("[");
        for (index, (parameter, argument)) in arguments.iter().enumerate() {
            if index > 0 {
                speller.text(", ");
            }
            if named {
                speller.text(&format!("{parameter}="));
            }
            speller.ty(*argument);
        }
        speller.text("]");
        let (parts, written) = speller.finish();

        Self {
            position,
            kind: InlayHintKind::CallTypeArguments,
            label: InlayHintLabel { parts },
            padding_left: false,
            padding_right: false,
            text_edits: if writable {
                insertion_edits(position, written, |text| text)
            } else {
                vec![]
            },
            placement: None,
            tooltip: None,
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
            placement: None,
            tooltip: None,
        }
    }

    /// The name of the type parameter a positional type argument fills.
    fn type_argument_name(position: TextSize, name: &str) -> Self {
        Self {
            position,
            kind: InlayHintKind::TypeArgumentName,
            label: InlayHintLabel {
                parts: vec![InlayHintLabelPart::new(name), "=".into()],
            },
            padding_left: false,
            padding_right: false,
            text_edits: vec![],
            placement: None,
            tooltip: None,
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
            placement: None,
            tooltip: None,
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
            placement: None,
            tooltip: None,
        }
    }

    /// basedpython: the default a parameter takes from the method its `def` overrides, shown
    /// where the parameter's own default would be written.
    ///
    /// `spaced` follows what the lowering writes into the signature, which follows python style:
    /// `a=1` for a bare parameter, `a: int = 1` for an annotated one.
    ///
    /// the value is a literal, which reads the same written here as in the base, so the hint
    /// writes itself. `inherited_from` names the method it comes from, which is the tooltip.
    fn inherited_default(
        position: TextSize,
        value: &str,
        spaced: bool,
        inherited_from: Option<&str>,
    ) -> Self {
        let written = if spaced {
            format!(" = {value}")
        } else {
            format!("={value}")
        };
        Self {
            position,
            kind: InlayHintKind::InheritedDefault,
            label: InlayHintLabel {
                parts: vec![
                    if spaced {
                        format!("= {value}")
                    } else {
                        format!("={value}")
                    }
                    .into(),
                ],
            },
            padding_left: spaced,
            padding_right: false,
            text_edits: vec![InlayHintTextEdit {
                range: TextRange::empty(position),
                new_text: written,
            }],
            placement: None,
            tooltip: inherited_from.map(|method| format!("Inherited from {}", inline_code(method))),
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
            placement: None,
            tooltip: None,
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
        kind: InlayHintKind,
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
            kind,
            label: InlayHintLabel { parts },
            padding_left: leading_space,
            padding_right: parameter_follows,
            text_edits: vec![],
            placement: None,
            tooltip: None,
        }
    }

    /// The return type recovered for a `def` that leaves its annotation out,
    /// shown where the annotation would be written.
    ///
    /// A `predicate` is shown as `-> place is T`, which says which place the call narrows where
    /// the `TypeIs[T]` it is typed as does not.
    fn inferred_return<'db>(
        context: InlayHintImportContext<'_, 'db>,
        function: &ast::StmtFunctionDef,
        predicate: Option<&InferredReturnPredicate<'db>>,
        returned: Type<'db>,
        type_params_in_scope: &[(BoundTypeVarInstance<'db>, Name)],
    ) -> Self {
        let position = function.parameters.end();
        let returned = predicate.map_or(returned, |predicate| predicate.narrowed);
        let details = type_display_details(
            context.db,
            context.file,
            returned,
            false,
            type_params_in_scope,
        );

        // each hint carries its own imports, because a client can apply one without the others
        let spelled = spell_type(
            context,
            &mut FxHashMap::default(),
            function.into(),
            function.start(),
            &details,
            type_params_in_scope,
        );

        // `x is not int` is a negated predicate, which is not what this is
        let (open, close) = match predicate {
            Some(predicate) if predicate.narrowed_reads_as_negation => ("(", ")"),
            _ => ("", ""),
        };
        let place = predicate.map(|predicate| format!("{} is {open}", predicate.place));
        let place = place.as_deref().unwrap_or_default();

        // a written `-> place is T` is typed as a `TypeIs`, which is read as being about the
        // first argument wherever it goes without its function, so only a predicate about that
        // argument is written that way unchanged
        let editable =
            spelled.editable && predicate.is_none_or(|predicate| predicate.is_type_is_parameter);
        let text_edits = if editable {
            let mut text_edits = vec![InlayHintTextEdit {
                range: TextRange::empty(position),
                new_text: format!(" -> {place}{}{close}", spelled.edit_label),
            }];
            text_edits.extend(spelled.import_edits);
            text_edits
        } else {
            vec![]
        };

        let mut label_parts = vec![format!("-> {place}").into()];
        label_parts.extend(spelled.label_parts);
        if !close.is_empty() {
            label_parts.push(close.into());
        }

        Self {
            position,
            kind: InlayHintKind::InferredReturnType,
            label: InlayHintLabel { parts: label_parts },
            padding_left: true,
            padding_right: false,
            text_edits,
            placement: None,
            tooltip: None,
        }
    }

    /// basedpython: the type arguments a generic base takes from the class
    /// body, where they would be written, with the edit that writes them there
    fn inferred_base_type_arguments<'db>(
        context: InlayHintImportContext<'_, 'db>,
        base: &Expr,
        arguments: &[BaseTypeArgument<'db>],
        type_params_in_scope: &[(BoundTypeVarInstance<'db>, Name)],
    ) -> Self {
        let position = base.end();
        let among: Vec<_> = arguments
            .iter()
            .map(|argument| match argument {
                BaseTypeArgument::Type(ty) | BaseTypeArgument::Unpacked(ty) => *ty,
            })
            .collect();
        let mut imports = FxHashMap::default();
        let mut speller = HintSpeller::new(
            context,
            &mut imports,
            base.into(),
            base.start(),
            type_params_in_scope,
            &among,
        );
        speller.text("[");
        for (index, argument) in arguments.iter().enumerate() {
            if index > 0 {
                speller.text(", ");
            }
            match argument {
                BaseTypeArgument::Type(ty) => speller.ty(*ty),
                BaseTypeArgument::Unpacked(ty) => {
                    speller.text("*");
                    speller.ty(*ty);
                }
            }
        }
        speller.text("]");
        let (parts, written) = speller.finish();

        Self {
            position,
            kind: InlayHintKind::InferredBaseTypeArguments,
            label: InlayHintLabel { parts },
            padding_left: false,
            padding_right: false,
            text_edits: insertion_edits(position, written, |text| text),
            placement: None,
            tooltip: None,
        }
    }

    /// the type of a place the source leaves unannotated — a parameter, or a
    /// basedpython property declaration — shown where the annotation would be
    /// written: `: int`, or the `some` annotation that would declare a
    /// parameter's inferred bound, `: some Sized`
    ///
    /// the edit is the caller's to place, see [`AnnotationSlot`], since where an
    /// annotation goes depends on what the source writes around it
    fn inferred_annotation<'db>(
        context: InlayHintImportContext<'_, 'db>,
        kind: InlayHintKind,
        slot: &AnnotationSlot<'_>,
        prefix: &str,
        ty: Type<'db>,
        type_params_in_scope: &[(BoundTypeVarInstance<'db>, Name)],
        tooltip: Option<String>,
    ) -> Self {
        let mut imports = FxHashMap::default();
        let mut speller = HintSpeller::new(
            context,
            &mut imports,
            slot.scope_node,
            slot.scope_node.start(),
            type_params_in_scope,
            &[],
        );
        speller.text(prefix);
        speller.ty(ty);
        let (parts, written) = speller.finish();

        Self {
            position: slot.position,
            kind,
            label: InlayHintLabel { parts },
            padding_left: false,
            padding_right: false,
            text_edits: written
                .map(|(annotation, imports)| {
                    let mut edits = slot.edits(context.db, context.file, &annotation);
                    edits.extend(imports);
                    edits
                })
                .unwrap_or_default(),
            placement: None,
            tooltip,
        }
    }

    pub fn display(&self) -> InlayHintDisplay<'_> {
        InlayHintDisplay { inlay_hint: self }
    }
}

/// what a hint in a python or basedpython file is
///
/// each kind is switched by a setting of [`InlayHintSettings`] of its own, so a
/// client can tell every kind apart by [`InlayHintKind::setting`] rather than
/// by reading its label. the django template hints have kinds of their own, in
/// `TemplateInlayHintKind`
///
/// what each kind is, as a client lists it — a title, a family, a description and an
/// example — is [`InlayHintKind::about`], in `inlay_hint_catalog`, which a kind added
/// here does not compile without
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumIter)]
pub enum InlayHintKind {
    /// The type of an unannotated binding
    VariableType,
    /// The type of an unannotated lambda parameter
    LambdaParameterType,
    /// basedpython: the type a parameter takes from the method it overrides or
    /// the overloads it implements
    InheritedParameterType,
    /// basedpython: the type a property declaration leaves to its accessors
    PropertyType,
    /// basedpython: the return type of a `def` that leaves its annotation out
    InferredReturnType,
    /// basedpython: the bound an unannotated parameter of a `def` takes from its body
    InferredParameterType,
    /// basedpython: the type arguments a generic base takes from the class body
    InferredBaseTypeArguments,
    CallArgumentName,
    /// basedpython: a function's inferred exception set
    Raises,
    /// basedpython: the variance inferred for a class type parameter
    Variance,
    /// basedpython: a type parameter reified without saying so
    Reification,
    /// The type arguments inferred for a generic call
    CallTypeArguments,
    /// The name of the type parameter a positional type argument fills
    TypeArgumentName,
    /// basedpython: a method that overrides a superclass member without saying so
    Override,
    /// basedpython-ui: the observables a function reads while composing
    Reads,
    /// basedpython-ui: `unstable` on a composable parameter the runtime cannot compare
    Stability,
    /// basedpython-ui: what a `derived(...)` computation depends on
    DerivedDeps,
    /// basedpython-ui: the composition scopes a state write invalidates
    Invalidates,
    /// The arms the typing spec's numeric promotion adds to `float` / `complex`
    NumericPromotion,
    /// The type a `reveal_type` call reveals
    RevealedType,
    /// The value an enum member takes without the source writing one
    EnumValue,
    /// basedpython: the parameters a trailing lambda binds without spelling them:
    /// `it`, and the receiver named `self` when the callback declares one
    ImplicitParameter,
    /// basedpython: the `self` an `init(...)` binds without spelling it
    ImplicitSelf,
    /// basedpython: an argument a call site fills from a `context` declaration
    ImplicitArgument,
    /// basedpython: the default a parameter takes from the method its `def` overrides
    InheritedDefault,
    /// in a runtime module, an annotation or the bases its stub declares for a declaration that
    /// does not spell them
    StubTypes,
    /// in a runtime module, a member its stub declares that the module does not, drawn as lines
    /// of their own
    StubOnlyMembers,
}

impl InlayHintKind {
    /// whether a client shows this kind of hint as a parameter rather than as a
    /// type, which are the only two kinds LSP has
    pub const fn is_parameter(self) -> bool {
        match self {
            InlayHintKind::VariableType
            | InlayHintKind::LambdaParameterType
            | InlayHintKind::InheritedParameterType
            | InlayHintKind::PropertyType
            | InlayHintKind::InferredReturnType
            | InlayHintKind::InferredParameterType
            | InlayHintKind::InferredBaseTypeArguments
            // basedpython: an inferred exception set is a type, like a return type
            | InlayHintKind::Raises
            | InlayHintKind::Variance
            | InlayHintKind::Reification
            | InlayHintKind::CallTypeArguments
            | InlayHintKind::TypeArgumentName
            | InlayHintKind::Override
            // basedpython-ui: a read set, a dependency set and an invalidation set
            // are typing facts about the function, and `unstable` is a modifier
            // like `override`
            | InlayHintKind::Reads
            | InlayHintKind::Stability
            | InlayHintKind::DerivedDeps
            | InlayHintKind::Invalidates
            | InlayHintKind::NumericPromotion
            | InlayHintKind::RevealedType
            | InlayHintKind::EnumValue
            | InlayHintKind::StubTypes
            | InlayHintKind::StubOnlyMembers => false,
            InlayHintKind::CallArgumentName
            | InlayHintKind::ImplicitParameter
            | InlayHintKind::ImplicitSelf
            | InlayHintKind::ImplicitArgument
            | InlayHintKind::InheritedDefault => true,
        }
    }
}

/// the one table saying which setting switches each kind of hint
///
/// from it come the kind's `setting()`, the `inlayHints` option a client knows
/// the setting by, and the `InlayHintSettings` methods that read and write the
/// field holding it, so a kind cannot be named by one setting and switched by
/// another. a client that lets its user treat each kind differently needs to
/// know which kind a hint is, and the label is what a hint says rather than
/// what it is: a revealed `list[int]` and a call's `[int]` differ by a bracket
macro_rules! hint_switches {
    (
        $kind:ident,
        $enabled:ident,
        $set_enabled:ident,
        { $($variant:ident => $field:ident = $setting:literal,)* }
    ) => {
        impl $kind {
            /// the name of the `inlayHints` option that switches this kind of hint
            pub const fn setting(self) -> &'static str {
                match self {
                    $($kind::$variant => $setting,)*
                }
            }
        }

        impl $crate::InlayHintSettings {
            /// whether hints of `kind` are shown
            pub const fn $enabled(&self, kind: $kind) -> bool {
                match kind {
                    $($kind::$variant => self.$field,)*
                }
            }

            /// show or hide hints of `kind`
            pub const fn $set_enabled(&mut self, kind: $kind, enabled: bool) {
                match kind {
                    $($kind::$variant => self.$field = enabled,)*
                }
            }
        }
    };
}

pub(crate) use hint_switches;

hint_switches!(InlayHintKind, enabled, set_enabled, {
    VariableType => variable_types = "variableTypes",
    LambdaParameterType => lambda_parameter_types = "lambdaParameterTypes",
    InheritedParameterType => inherited_parameter_types = "inheritedParameterTypes",
    PropertyType => property_types = "propertyTypes",
    InferredReturnType => inferred_return_types = "returnTypes",
    InferredParameterType => inferred_parameter_types = "inferredParameterTypes",
    InferredBaseTypeArguments => inferred_base_type_arguments = "inferredBaseTypeArguments",
    CallArgumentName => call_argument_names = "callArgumentNames",
    Raises => inferred_raises = "inferredRaises",
    Variance => inferred_variance = "inferredVariance",
    Reification => inferred_reification = "inferredReification",
    CallTypeArguments => call_type_arguments = "callTypeArguments",
    TypeArgumentName => type_argument_names = "typeArgumentNames",
    Override => inferred_override = "inferredOverride",
    Reads => inferred_reads = "inferredReads",
    Stability => parameter_stability = "parameterStability",
    DerivedDeps => derived_dependencies = "derivedDependencies",
    Invalidates => inferred_invalidations = "inferredInvalidations",
    NumericPromotion => numeric_promotions = "numericPromotions",
    RevealedType => revealed_types = "revealedTypes",
    EnumValue => enum_values = "enumValues",
    ImplicitParameter => implicit_parameters = "implicitParameters",
    ImplicitSelf => implicit_self = "implicitSelf",
    ImplicitArgument => implicit_arguments = "implicitArguments",
    InheritedDefault => inherited_parameter_defaults = "inheritedParameterDefaults",
    StubTypes => stub_types = "stubTypes",
    StubOnlyMembers => stub_only_members = "stubOnlyMembers",
});

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

    /// markdown about this part alone, which a client shows over it rather than over the hint
    tooltip: Option<String>,
}

impl InlayHintLabelPart {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            target: None,
            tooltip: None,
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

    pub fn tooltip(&self) -> Option<&str> {
        self.tooltip.as_deref()
    }

    pub fn with_tooltip(self, tooltip: Option<String>) -> Self {
        Self { tooltip, ..self }
    }
}

impl From<String> for InlayHintLabelPart {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<&str> for InlayHintLabelPart {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

/// The parts of a hint that lists observables, or the scopes that observe
/// one: `prefix`, then each name as a part navigable to its declaration,
/// comma-separated, and `…` when `opaque` — something the analysis could not
/// follow.
fn state_read_parts(prefix: &str, reads: &[StateRead], opaque: bool) -> Vec<InlayHintLabelPart> {
    let mut parts: Vec<InlayHintLabelPart> = vec![prefix.into()];
    for (index, read) in reads.iter().enumerate() {
        if index > 0 {
            parts.push(", ".into());
        }
        parts.push(
            InlayHintLabelPart::new(read.name.as_str())
                .with_target(Some(NavigationTarget::from(read.declaration))),
        );
    }
    if opaque {
        if !reads.is_empty() {
            parts.push(", ".into());
        }
        parts.push("…".into());
    }
    parts
}

#[derive(Debug, Clone)]
pub struct InlayHintTextEdit {
    pub range: TextRange,
    pub new_text: String,
}

/// the edits that write a hint spelled as `written` at `position`, the text it inserts made by
/// `insert` from what the speller wrote, followed by the imports it needs. none when the hint
/// cannot be written
fn insertion_edits(
    position: TextSize,
    written: Option<(String, Vec<InlayHintTextEdit>)>,
    insert: impl FnOnce(String) -> String,
) -> Vec<InlayHintTextEdit> {
    let Some((text, imports)) = written else {
        return vec![];
    };
    std::iter::once(InlayHintTextEdit {
        range: TextRange::empty(position),
        new_text: insert(text),
    })
    .chain(imports)
    .collect()
}

/// `text` as markdown inline code, fenced by more backticks than it holds in a row
fn inline_code(text: &str) -> String {
    let longest = text
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    // a span that starts or ends with a backtick needs a space between it and the fence
    if text.starts_with('`') || text.ends_with('`') {
        format!("{fence} {text} {fence}")
    } else {
        format!("{fence}{text}{fence}")
    }
}

/// the tooltip of an inferred `raises` hint: each place in the body something escapes from, as
/// its line, the first line of the statement or call there, and what escapes from it. `None` when
/// there are none to list
fn raise_sites_tooltip<'db>(
    source: &str,
    line_index: &ruff_source_file::LineIndex,
    sites: &[(TextRange, Type<'db>)],
    display: impl Fn(Type<'db>) -> String,
) -> Option<String> {
    use std::fmt::Write as _;

    // a long body lists what a reader can take in; the rest is a count
    const SHOWN: usize = 8;
    // a site is quoted by its first line, cut short past this many characters
    const QUOTED: usize = 60;

    if sites.is_empty() {
        return None;
    }
    let mut tooltip = String::from("Raised by:");
    for (range, raised) in sites.iter().take(SHOWN) {
        let line = line_index.line_index(range.start());
        let text = source.get(range.to_std_range()).unwrap_or_default();
        let first_line = text.lines().next().unwrap_or_default().trim_end();
        let quoted: String = if first_line.chars().count() > QUOTED || first_line.len() < text.len()
        {
            let mut cut: String = first_line.chars().take(QUOTED).collect();
            cut.push('…');
            cut
        } else {
            first_line.to_string()
        };
        let _ = write!(
            tooltip,
            "\n- line {line}, {}: {}",
            inline_code(&quoted),
            inline_code(&display(*raised))
        );
    }
    if sites.len() > SHOWN {
        let _ = write!(tooltip, "\n- and {} more", sites.len() - SHOWN);
    }
    Some(tooltip)
}

/// where an annotation a parameter or a property declaration leaves out would be written
struct AnnotationSlot<'a> {
    /// after the name it annotates
    position: TextSize,
    /// the node whose scope the annotation is read in: the `def` or `lambda` a parameter belongs
    /// to, or a property's getter
    scope_node: AnyNodeRef<'a>,
    /// the parameter's default, which an annotation is written before with spaces around the `=`
    /// it would otherwise go without: `a=1` takes `: int` as `a: int = 1`
    default: Option<&'a Expr>,
    /// the parameters of a basedpython lambda written without the parentheses an annotated one
    /// needs: `lambda (n: int): ...`
    parenthesize: Option<TextRange>,
}

impl AnnotationSlot<'_> {
    /// the edits writing `annotation`, `: int`, into the slot
    fn edits(
        &self,
        db: &dyn Db,
        file: ProgramFile<'_>,
        annotation: &str,
    ) -> Vec<InlayHintTextEdit> {
        let source = source_text(db, file.file(db));
        let mut edits = Vec::new();
        if let Some(parameters) = self.parenthesize {
            edits.push(InlayHintTextEdit {
                range: TextRange::empty(parameters.start()),
                new_text: "(".to_string(),
            });
        }
        // only the `=` and the spaces around it are replaced: anything else between the name and
        // the default, a comment or a line break, stays as it was, and the annotation goes in
        // unspaced before it
        let respaced = self.default.and_then(|default| {
            let between = TextRange::new(self.position, default.start());
            let text = source.get(between.to_std_range())?;
            (text.trim_matches([' ', '\t']) == "=").then_some(between)
        });
        edits.push(match respaced {
            Some(between) => InlayHintTextEdit {
                range: between,
                new_text: format!("{annotation} = "),
            },
            None => InlayHintTextEdit {
                range: TextRange::empty(self.position),
                new_text: annotation.to_string(),
            },
        });
        if let Some(parameters) = self.parenthesize {
            edits.push(InlayHintTextEdit {
                range: TextRange::empty(parameters.end()),
                new_text: ")".to_string(),
            });
        }
        edits
    }
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
    let source = source_text(db, file.file(db));
    let importer = import_order::importer(db, file, &ast);

    let mut visitor = InlayHintVisitor::new(db, file, importer, source.as_str(), range, settings);

    visitor.add_stub_only_members(&ast);
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
    ///
    /// in a basedpython file it also names the type arguments of the types the
    /// variable type and call type argument hints show, which are still shown
    /// and hidden by their own settings
    ///
    /// ```by
    /// x": dict[Key=str, Value=int]" = identity({"a": 1})
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

    /// basedpython-ui: whether to show the observables a function reads while
    /// composing, after its return annotation and `raises` clause.
    ///
    /// ```by
    /// @composable
    /// def Counter(step: int = 1)" reads count":
    ///     let count = state(0)
    ///     Text(f"{count.value}")
    /// ```
    pub inferred_reads: bool,

    /// basedpython-ui: whether to show `unstable` before a composable parameter
    /// whose type the runtime cannot compare, so the composable is never
    /// skipped. A stable parameter gets no hint.
    ///
    /// ```by
    /// @composable
    /// def TodoList("unstable "items: list[str]): ...
    /// ```
    pub parameter_stability: bool,

    /// basedpython-ui: whether to show what a `derived(...)` or `remember(...)`
    /// computation depends on, at the end of the call's line.
    ///
    /// ```by
    /// let full = derived(lambda: name.value + email.value)" depends on name, email"
    /// ```
    pub derived_dependencies: bool,

    /// basedpython-ui: whether to show the composition scopes a state write
    /// made after composing — in a handler, a lambda, a nested `def`, an
    /// effect — invalidates, at the end of the write's line: the composables
    /// and `derived` computations that read what is written. A write nobody
    /// observes shows `nothing`.
    ///
    /// ```by
    /// Button("+"):
    ///     count.value += step" invalidates Counter"
    /// ```
    pub inferred_invalidations: bool,

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

    /// basedpython: whether to show the default a parameter takes from the method
    /// its `def` overrides.
    ///
    /// ```by
    /// class B(A):
    ///     def f(self, a"=1"): ...
    /// ```
    pub inherited_parameter_defaults: bool,

    /// Whether to show the return type recovered for a `def` that leaves its
    /// annotation out, which ty recovers when `analysis.return-type-inference` or
    /// `analysis.sound-types` asks it to.
    ///
    /// ```by
    /// def f()" -> 1":
    ///     return 1
    /// ```
    pub inferred_return_types: bool,

    /// basedpython: whether to show the bound recovered for an unannotated
    /// parameter of a `def` from what its body does with it
    ///
    /// ```by
    /// def f(a": some protocol(let foo: object)"):
    ///     _ = a.foo
    /// ```
    pub inferred_parameter_types: bool,

    /// basedpython: whether to show the type arguments a generic base written
    /// without them takes from the members the class body overrides
    ///
    /// ```by
    /// class B(A"[int]"):
    ///     x = 1
    /// ```
    pub inferred_base_type_arguments: bool,

    /// basedpython: whether to show the type a property declaration leaves to its
    /// accessors.
    ///
    /// ```by
    /// class A:
    ///     let a": 1"
    ///         get() = 1
    /// ```
    pub property_types: bool,

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

    /// in a runtime module with a stub, whether to show the parameter annotations,
    /// return annotation and bases the stub declares for a declaration that does
    /// not spell them, where they would be written. a slot this hint covers gets
    /// no inferred, inherited or return type hint beside it.
    ///
    /// ```python
    /// def f(a": int")" -> None": ...
    /// ```
    pub stub_types: bool,

    /// in a runtime module with a stub, whether to show each member the stub
    /// declares and the module does not, as the stub writes it, on lines of its
    /// own where the stub's order puts it.
    pub stub_only_members: bool,

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
    pub(crate) fn none() -> Self {
        Self {
            variable_types: false,
            call_argument_names: false,
            inferred_raises: false,
            inferred_variance: false,
            inferred_reification: false,
            call_type_arguments: false,
            type_argument_names: false,
            inferred_override: false,
            inferred_reads: false,
            parameter_stability: false,
            derived_dependencies: false,
            inferred_invalidations: false,
            numeric_promotions: false,
            revealed_types: false,
            implicit_parameters: false,
            implicit_self: false,
            lambda_parameter_types: false,
            inherited_parameter_types: false,
            inherited_parameter_defaults: false,
            inferred_return_types: false,
            inferred_parameter_types: false,
            inferred_base_type_arguments: false,
            property_types: false,
            implicit_arguments: false,
            enum_values: false,
            stub_types: false,
            stub_only_members: false,
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
            inferred_reads,
            parameter_stability,
            derived_dependencies,
            inferred_invalidations,
            numeric_promotions,
            revealed_types,
            implicit_parameters,
            implicit_self,
            lambda_parameter_types,
            inherited_parameter_types,
            inherited_parameter_defaults,
            inferred_return_types,
            inferred_parameter_types,
            inferred_base_type_arguments,
            property_types,
            implicit_arguments,
            enum_values,
            stub_types,
            stub_only_members,
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
            || inferred_reads
            || parameter_stability
            || derived_dependencies
            || inferred_invalidations
            || numeric_promotions
            || revealed_types
            || implicit_parameters
            || implicit_self
            || lambda_parameter_types
            || inherited_parameter_types
            || inherited_parameter_defaults
            || inferred_return_types
            || inferred_parameter_types
            || inferred_base_type_arguments
            || property_types
            || implicit_arguments
            || enum_values
            || stub_types
            || stub_only_members
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
            inferred_reads: true,
            parameter_stability: true,
            derived_dependencies: true,
            inferred_invalidations: true,
            numeric_promotions: true,
            revealed_types: true,
            implicit_parameters: true,
            implicit_self: true,
            lambda_parameter_types: true,
            inherited_parameter_types: true,
            inherited_parameter_defaults: true,
            inferred_return_types: true,
            inferred_parameter_types: true,
            inferred_base_type_arguments: true,
            property_types: true,
            implicit_arguments: true,
            enum_values: true,
            stub_types: true,
            stub_only_members: true,
            template_binding_types: true,
            resolved_templates: true,
        }
    }
}

/// `ty` rendered to a string, with the spans of the types that make it up. The
/// type parameters in `type_params_in_scope` are written by the names they come with.
fn type_display_details<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    ty: Type<'db>,
    named_type_arguments: bool,
    type_params_in_scope: &[(BoundTypeVarInstance<'db>, Name)],
) -> TypeDisplayDetails<'db> {
    type_display_details_among(
        db,
        file,
        ty,
        &[],
        named_type_arguments,
        type_params_in_scope,
    )
}

/// [`type_display_details`] for `ty` shown beside the types in `among`, so that a class whose
/// name one of them shares is qualified in both
fn type_display_details_among<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    ty: Type<'db>,
    among: &[Type<'db>],
    named_type_arguments: bool,
    type_params_in_scope: &[(BoundTypeVarInstance<'db>, Name)],
) -> TypeDisplayDetails<'db> {
    let env = ProgramEnvironment::from_file(file);
    let settings = DisplaySettings::from_possibly_ambiguous_types(
        db,
        &env,
        std::iter::once(ty).chain(among.iter().copied()),
    )
    .with_hidden_type_variable_scopes()
    .with_type_params_in_scope(db, type_params_in_scope.iter().cloned());
    let settings = if named_type_arguments {
        settings.with_named_type_arguments()
    } else {
        settings
    };
    // basedpython: a symbolic operation such as a repeated tuple, `(1, "a") * int`, has no
    // python spelling, so a hint in a python file shows the type it reduces to — which that
    // file can also hold as an annotation
    let settings = if file.file(db).source_type(db).is_basedpython() {
        settings
    } else {
        settings.with_reduced_symbolic_operations()
    };
    ty.display_with(db, &env, settings).to_string_parts()
}

/// A type as a hint shows it and as an annotation would write it.
struct SpelledType {
    /// The rendering, each type in it linked to its definition.
    label_parts: Vec<InlayHintLabelPart>,
    /// The annotation to write, which qualifies a name where importing it
    /// would clash with one in scope.
    edit_label: String,
    /// The imports the annotation needs.
    import_edits: Vec<InlayHintTextEdit>,
    /// Whether the annotation can be written at all: it is valid syntax, and
    /// every type it names can be reached from the file.
    editable: bool,
    /// The imports this spelling added to the ones the context already held,
    /// so that a caller dropping the spelling can drop them too.
    imported: Vec<DynamicallyImportedMember>,
}

/// Spell `details` for the scope around `scope_node`, importing what it names.
///
/// `dynamic_imports` holds the imports already made for the edits this spelling
/// is applied together with, so that a name they import is not imported twice.
///
/// A type parameter in `type_params_in_scope` is written by the name it comes
/// with, with nothing imported for it: one that what is being written declares
/// itself, or that a scope around it declares.
fn spell_type<'db>(
    context: InlayHintImportContext<'_, 'db>,
    dynamic_imports: &mut FxHashMap<DynamicallyImportedMember, ImportAction>,
    scope_node: AnyNodeRef<'_>,
    scope_offset: TextSize,
    details: &TypeDisplayDetails<'db>,
    type_params_in_scope: &[(BoundTypeVarInstance<'db>, Name)],
) -> SpelledType {
    let mut dynamic_importer = DynamicImporter::new(
        context.importer,
        context.scope_members,
        scope_node,
        scope_offset,
        dynamic_imports,
    );
    let (label_parts, edit_label, editable) = spell_type_with(
        context,
        &mut dynamic_importer,
        details,
        type_params_in_scope,
    );
    SpelledType {
        label_parts,
        edit_label,
        import_edits: dynamic_importer.text_edits(),
        editable,
        imported: dynamic_importer.imported_members,
    }
}

/// [`spell_type`] with the imports made through `dynamic_importer`, which the caller turns into
/// edits once it has spelled everything they are applied together with. answers the label's
/// parts, the annotation to write, and whether it can be written
fn spell_type_with<'db>(
    context: InlayHintImportContext<'_, 'db>,
    dynamic_importer: &mut DynamicImporter<'_, 'db>,
    details: &TypeDisplayDetails<'db>,
    type_params_in_scope: &[(BoundTypeVarInstance<'db>, Name)],
) -> (Vec<InlayHintLabelPart>, String, bool) {
    let InlayHintImportContext { db, file, .. } = context;
    let env = ProgramEnvironment::from_file(file);
    let mut editable = details.is_valid_syntax;

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

    let mut label_parts = Vec::new();
    for (target, detail) in details.targets.iter().zip(&details.details) {
        match detail {
            TypeDetail::Type(ty) => {
                // `Unknown` is what ty says where it knows nothing, not a type a program can
                // name: `ty_extensions` exists only for the checker, so an annotation naming it
                // fails to import when the program runs, and one that held would claim a
                // gradual type the author never chose. a type mentioning it has no annotation
                if ty.is_unknown() {
                    editable = false;
                }
                // basedpython: an unannotated parameter's inferred type parameter is written by
                // the parameter's name, which only names it where the parameter writes `some`
                if is_inferred_parameter_hole(db, *ty) {
                    editable = false;
                }
                let start = target.start().to_usize();
                let end = target.end().to_usize();
                // If we skipped over some bytes, push them with no target
                if start > offset {
                    label_parts.push(details.label[offset..start].into());
                }

                // Possibly import the current type and return the qualified name
                let mut qualified_name = |dynamic_importer: &mut DynamicImporter<'_, 'db>| {
                    if let Type::TypeVar(typevar) = ty
                        && type_params_in_scope
                            .iter()
                            .any(|(param, _)| param.is_same_occurrence(db, *typevar))
                    {
                        return None;
                    }
                    let type_definition = ty.definition(db, &env)?;
                    let definition = type_definition.definition()?;

                    // Only module-level names can be imported with `from <module> import <name>`.
                    // If the definition lives in a class or function body we can't produce a safe edit.
                    if !definition.file_scope(db).is_global() {
                        editable = false;
                        return None;
                    }

                    let definition_name = definition.name(db);

                    // Fallback to the label if we cannot find the name
                    let definition_name = definition_name
                        .as_deref()
                        .unwrap_or(&details.label[start..end]);

                    // basedpython: a name a `.by` file reaches without importing it, which is
                    // imported from the module the language takes it from when the file binds
                    // the name to something else
                    let implicit_module =
                        implicit_type_name_module(db, &env, file, definition_name, *ty);

                    let module = file_to_module(db, definition.program_file(db).resolver_file(db))?;

                    // Types like `ty_extensions.Unknown` only exist in stubs and cannot be
                    // imported at runtime, so we do not provide edits.
                    //
                    // basedpython: a name the file reaches without importing it is imported, if
                    // at all, from the module the language takes it from
                    if implicit_module.is_none() && module.is_known_stub_only(db) {
                        editable = false;
                        return None;
                    }

                    // Don't try to import symbols in scope
                    let definition_file = definition.file(db);
                    if definition_file == file.file(db) {
                        return None;
                    }

                    if should_skip_import(db, module, *ty) {
                        return None;
                    }

                    let module_name = implicit_module.unwrap_or(module.name(db).as_str());

                    dynamic_importer.import_symbol(
                        db,
                        &env,
                        ty,
                        module_name,
                        definition_name,
                        &details.label[start..end],
                        implicit_module.is_some(),
                    )
                };

                // Ok, this is the first type that claimed these bytes, give it the target
                if start >= offset {
                    // Try to import the symbol and update the edit label if required
                    if let Some(qualified_name) = qualified_name(dynamic_importer) {
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
            TypeDetail::SignatureStart | TypeDetail::SignatureEnd | TypeDetail::Parameter(_) => {
                // Don't care about these
            }
        }
    }

    // "flush" the rest of the label without any target
    if offset < details.label.len() {
        label_parts.push(details.label[offset..details.label.len()].into());
    }

    (label_parts, edit_label, editable)
}

/// a hint's label and the source that writes it, built side by side, piece by piece: text,
/// which reads the same in both, and types, which the label shows as the file displays them, each
/// linked to its definition, and the source writes as an annotation of the file, importing what it
/// names
///
/// one hint is applied as a whole, so its imports are made once for all of its types, and it can
/// be written only when every type in it can
struct HintSpeller<'a, 'db> {
    context: InlayHintImportContext<'a, 'db>,
    importer: DynamicImporter<'a, 'db>,
    type_params_in_scope: &'a [(BoundTypeVarInstance<'db>, Name)],
    /// every type the hint shows, so that two of them sharing a name are told apart in both
    among: &'a [Type<'db>],
    parts: Vec<InlayHintLabelPart>,
    written: String,
    editable: bool,
}

impl<'a, 'db> HintSpeller<'a, 'db> {
    /// a speller for a hint in the scope around `scope_node`, where the types in
    /// `type_params_in_scope` are written by the names they come with
    fn new(
        context: InlayHintImportContext<'a, 'db>,
        dynamic_imports: &'a mut FxHashMap<DynamicallyImportedMember, ImportAction>,
        scope_node: AnyNodeRef<'a>,
        scope_offset: TextSize,
        type_params_in_scope: &'a [(BoundTypeVarInstance<'db>, Name)],
        among: &'a [Type<'db>],
    ) -> Self {
        Self {
            context,
            importer: DynamicImporter::new(
                context.importer,
                context.scope_members,
                scope_node,
                scope_offset,
                dynamic_imports,
            ),
            type_params_in_scope,
            among,
            parts: Vec::new(),
            written: String::new(),
            editable: true,
        }
    }

    /// `text`, as both the label and the source write it
    fn text(&mut self, text: &str) {
        self.parts.push(text.into());
        self.written.push_str(text);
    }

    /// `ty`, as the label shows it and an annotation of the file writes it
    fn ty(&mut self, ty: Type<'db>) {
        let InlayHintImportContext { db, file, .. } = self.context;
        let details =
            type_display_details_among(db, file, ty, self.among, false, self.type_params_in_scope);
        let (parts, written, editable) = spell_type_with(
            self.context,
            &mut self.importer,
            &details,
            self.type_params_in_scope,
        );
        self.parts.extend(parts);
        self.written.push_str(&written);
        self.editable &= editable;
    }

    /// the label, and what writes the hint: the text to put at its position and the imports it
    /// needs, `None` when a type in it cannot be written
    fn finish(
        self,
    ) -> (
        Vec<InlayHintLabelPart>,
        Option<(String, Vec<InlayHintTextEdit>)>,
    ) {
        let written = self
            .editable
            .then(|| (self.written, self.importer.text_edits()));
        (self.parts, written)
    }
}

/// `ty` written as an annotation in `file`, for a position in the scope around
/// `scope_node`, with the imports it needs; `None` when no annotation the file
/// could hold says it.
///
/// A type parameter in `type_params_in_scope` is written by the name it comes
/// with: see [`spell_type`].
pub(crate) fn annotation_for_type<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    scope_node: AnyNodeRef<'_>,
    scope_offset: TextSize,
    ty: Type<'db>,
    type_params_in_scope: &[(BoundTypeVarInstance<'db>, Name)],
) -> Option<(String, Vec<InlayHintTextEdit>)> {
    let ast = parsed_module(db, file.python_file(db)).load(db);
    let importer = import_order::importer(db, file, &ast);
    // a basedpython file writes types in basedpython's own spelling
    with_display_for_file(db, file.file(db), || {
        let details = type_display_details(db, file, ty, false, type_params_in_scope);
        let spelled = spell_type(
            InlayHintImportContext {
                db,
                file,
                importer: &importer,
                scope_members: None,
            },
            &mut FxHashMap::default(),
            scope_node,
            scope_offset,
            &details,
            type_params_in_scope,
        );
        spelled
            .editable
            .then_some((spelled.edit_label, spelled.import_edits))
    })
}

/// Types written as annotations in one file, importing what they name. See
/// [`with_annotation_speller`].
pub(crate) struct AnnotationSpeller<'a, 'db> {
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    importer: &'a Importer<'a>,
    dynamic_imports: FxHashMap<DynamicallyImportedMember, ImportAction>,
    import_edits: Vec<InlayHintTextEdit>,
}

impl<'db> AnnotationSpeller<'_, 'db> {
    /// `ty` written as an annotation for a position in the scope around
    /// `scope_node`, or `None` when no annotation the file could hold says it.
    /// A type parameter in `type_params_in_scope` is written by the name it comes
    /// with, with nothing imported for it: see [`spell_type`].
    pub(crate) fn spell(
        &mut self,
        scope_node: AnyNodeRef<'_>,
        scope_offset: TextSize,
        ty: Type<'db>,
        type_params_in_scope: &[(BoundTypeVarInstance<'db>, Name)],
    ) -> Option<String> {
        let db = self.db;
        let file = self.file;
        // a basedpython file writes types in basedpython's own spelling
        let spelled = with_display_for_file(db, file.file(db), || {
            let details = type_display_details(db, file, ty, false, type_params_in_scope);
            spell_type(
                InlayHintImportContext {
                    db,
                    file,
                    importer: self.importer,
                    scope_members: None,
                },
                &mut self.dynamic_imports,
                scope_node,
                scope_offset,
                &details,
                type_params_in_scope,
            )
        });
        if !spelled.editable {
            // an import is remembered so that a second annotation naming the same
            // type does not import it again. one remembered for an annotation
            // that is not written would leave the second without its import
            for member in &spelled.imported {
                self.dynamic_imports.remove(member);
            }
            return None;
        }
        self.import_edits.extend(spelled.import_edits);
        Some(spelled.edit_label)
    }

    /// `name`, which `module` binds at its top level, or the module itself when there is no
    /// `name`, written for a position in the scope around `scope_node` and imported there: by
    /// its name, or through its module when the file binds the name to something else.
    pub(crate) fn reference(
        &mut self,
        scope_node: AnyNodeRef<'_>,
        scope_offset: TextSize,
        module: &str,
        name: Option<&str>,
    ) -> String {
        let mut importer = DynamicImporter::new(
            self.importer,
            None,
            scope_node,
            scope_offset,
            &mut self.dynamic_imports,
        );
        let text = importer.import_name(module, name);
        let edits = importer.text_edits();
        self.import_edits.extend(edits);
        text
    }
}

/// Runs `spell` with an [`AnnotationSpeller`] for `file`, and answers what it
/// answered together with the imports the annotations it wrote need, each once.
pub(crate) fn with_annotation_speller<'db, R>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    spell: impl FnOnce(&mut AnnotationSpeller<'_, 'db>) -> R,
) -> (R, Vec<InlayHintTextEdit>) {
    let ast = parsed_module(db, file.python_file(db)).load(db);
    let importer = import_order::importer(db, file, &ast);
    let mut speller = AnnotationSpeller {
        db,
        file,
        importer: &importer,
        dynamic_imports: FxHashMap::default(),
        import_edits: Vec::new(),
    };
    let answer = spell(&mut speller);
    (answer, speller.import_edits)
}

#[derive(Clone, Copy)]
struct InlayHintImportContext<'a, 'db> {
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    importer: &'a Importer<'db>,
    /// the members in scope in each scope the hints of one request import into, when they are
    /// kept across hints
    scope_members: Option<&'a ScopeMembers<'db>>,
}

/// the members in scope in each scope one request's hints import into
///
/// what is in scope depends only on the scope, and building it reads the type of every name the
/// scope and the scopes around it bind, builtins included. one request asks for it once per hint
/// that names a type, hundreds of times in a large file, where it measured a third of the request
struct ScopeMembers<'db> {
    model: SemanticModel<'db>,
    scopes: std::cell::RefCell<FxHashMap<Option<FileScopeId>, MembersInScope<'db>>>,
}

impl<'db> ScopeMembers<'db> {
    fn new(db: &'db dyn Db, file: ProgramFile<'db>) -> Self {
        Self {
            model: SemanticModel::new(db, file),
            scopes: std::cell::RefCell::default(),
        }
    }

    /// the members in scope at `node`, for an import needed at `at`
    fn at(
        &self,
        importer: &Importer<'db>,
        node: AnyNodeRef<'_>,
        at: TextSize,
    ) -> MembersInScope<'db> {
        let scope = self.model.scope(node);
        if let Some(members) = self.scopes.borrow().get(&scope) {
            return members.at(at);
        }
        let members = importer.members_in_scope_at(node, at);
        self.scopes.borrow_mut().insert(scope, members.clone());
        members
    }
}

struct InlayHintVisitor<'a, 'db> {
    db: &'db dyn Db,
    model: SemanticModel<'db>,
    importer: Importer<'db>,
    scope_members: ScopeMembers<'db>,
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
    /// The `lambda` whose parameter list or body we are inside, if any.
    lambda: Option<&'a ast::ExprLambda>,
    /// The `def` whose signature or body we are inside, if any: the one a parameter visited
    /// outside a lambda belongs to.
    signature_of: Option<&'a ast::StmtFunctionDef>,
    /// The default of the parameter being visited, if it writes one.
    parameter_default: Option<&'a Expr>,
    /// Whether the call being visited is the callee of a basedpython trailing lambda, which the
    /// lowering passes the block to as its last positional argument.
    in_trailing_lambda_callee: bool,
    /// The operand of a union in a type expression currently being visited, and what the
    /// operands written beside it denote. Read by numeric promotion, which adds nothing an
    /// operand already spells.
    union_siblings: Option<(TextRange, Vec<Type<'db>>)>,
    /// The class whose body we are directly inside, if any.
    enclosing_class: Option<Type<'db>>,
    /// The stub describing this module, when one does and a hint drawn from it is on.
    stub: Option<stub::StubContext<'db>>,
    /// The positions of the annotation slots a `stubTypes` hint filled, which no other hint
    /// fills beside it.
    stub_covered: rustc_hash::FxHashSet<TextSize>,
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
            importer,
            scope_members: ScopeMembers::new(db, file),
            source,
            hints: Vec::new(),
            assignment_rhs: None,
            range,
            settings,
            in_no_edits_allowed: false,
            source_type: file.file(db).source_type(db),
            in_type_expression: false,
            lambda: None,
            signature_of: None,
            parameter_default: None,
            in_trailing_lambda_callee: false,
            union_siblings: None,
            enclosing_class: None,
            stub: (settings.stub_types || settings.stub_only_members)
                .then(|| stub::StubContext::new(db, file))
                .flatten(),
            stub_covered: rustc_hash::FxHashSet::default(),
        }
    }

    /// Whether the file uses basedpython syntax, which several hints spell.
    fn is_basedpython(&self) -> bool {
        self.source_type.is_basedpython()
    }

    /// add `hint` if the setting of its kind is on, answering whether it was added
    ///
    /// every hint is added here, so a hint is shown exactly when the one setting
    /// its kind names is on. the same check made before a hint is worked out
    /// only saves the work of one that would be dropped
    fn push(&mut self, hint: InlayHint) -> bool {
        if !self.settings.enabled(hint.kind) {
            return false;
        }
        self.hints.push(hint);
        true
    }

    fn add_type_hint(&mut self, expr: &Expr, rhs: &Expr, ty: Type<'db>, allow_edits: bool) {
        if !self.settings.enabled(InlayHintKind::VariableType) {
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

        let context = self.import_context();

        let type_params_in_scope = self.model.type_params_in_scope(expr.into());
        if let Some(inlay_hint) = InlayHint::variable_type(
            context,
            expr,
            rhs,
            ty,
            allow_edits,
            named_type_arguments,
            &type_params_in_scope,
        ) {
            self.push(inlay_hint);
        }
    }

    /// basedpython: hint the exception set of a function with no `raises` clause.
    ///
    /// The hint sits where the clause would be written — after the return
    /// annotation, before the `:` — so accepting it reads as ordinary source.
    fn add_inferred_raises(&mut self, function: &ast::StmtFunctionDef) {
        let env = &self.model.program_environment();
        if !self.settings.enabled(InlayHintKind::Raises) || function.raises.is_some() {
            return;
        }

        let Some(function_type) = function.inferred_type(&self.model) else {
            return;
        };
        let Some(raised) = inferred_raises(self.db, env, function_type) else {
            return;
        };

        let position = function
            .returns
            .as_deref()
            .map_or_else(|| function.parameters.end(), Ranged::end);

        let tooltip = raise_sites_tooltip(
            self.source,
            &line_index(self.db, self.model.file()),
            &inferred_raise_sites(self.db, function_type),
            |ty| ty.display(self.db, env).to_string(),
        );
        let context = self.import_context();
        let type_params_in_scope = self.model.type_params_in_signature(function);
        self.push(InlayHint::inferred_raises(
            context,
            function,
            position,
            raised,
            &type_params_in_scope,
            tooltip,
        ));
    }

    /// basedpython: hint the variance ty infers for each type parameter of
    /// `class` that does not declare one.
    ///
    /// `reified` names the type parameters the owner reifies without saying so, which may not
    /// declare a variance any more than one that says so may.
    fn add_inferred_variances(
        &mut self,
        type_params: Option<&ast::TypeParams>,
        reified: &[Name],
        owner: impl FnOnce(&SemanticModel<'db>) -> Option<Type<'db>>,
    ) {
        if !self.settings.enabled(InlayHintKind::Variance) || !self.is_basedpython() {
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

            // the keyword goes before the name, after a `reified` the parameter writes. a pack
            // has no keyword, so its hint stays where the parameter starts
            let (position, writable) = match type_param {
                ast::TypeParam::TypeVar(type_var) => (
                    type_var.name.start(),
                    !type_var.is_reified && !reified.contains(&type_var.name.id),
                ),
                ast::TypeParam::TypeVarTuple(_) | ast::TypeParam::ParamSpec(_) => {
                    (type_param.range().start(), false)
                }
            };
            self.push(InlayHint::inferred_variance(position, variance, writable));
        }
    }

    /// basedpython: hint `reified` on each type parameter of `function` that a
    /// value-position use in the body, or what the body forwards it into, reifies
    /// without saying so.
    fn add_inferred_reification(&mut self, function: &ast::StmtFunctionDef) {
        let inferred = |hints: &mut Self| {
            let mut names = inferred_reified_type_param_names(hints.source_type, function);
            names.extend_from_slice(&hints.model.forwarded_type_params(function).names);
            names
        };
        self.add_inferred_reification_of(function.type_params.as_deref(), inferred);
    }

    /// basedpython: the same hint on a class, which reifies a type parameter its
    /// methods read through their receiver. answers the names it reifies so.
    fn add_inferred_class_reification(&mut self, class: &ast::StmtClassDef) -> Vec<Name> {
        let inferred =
            |hints: &mut Self| inferred_reified_class_type_param_names(hints.source_type, class);
        self.add_inferred_reification_of(class.type_params.as_deref(), inferred)
    }

    /// answers the names reified without saying so, hinted or not
    fn add_inferred_reification_of(
        &mut self,
        type_params: Option<&ast::TypeParams>,
        inferred: impl FnOnce(&mut Self) -> Vec<Name>,
    ) -> Vec<Name> {
        if !self.is_basedpython() {
            return Vec::new();
        }

        let Some(type_params) = type_params else {
            return Vec::new();
        };

        let inferred = inferred(self);

        if !self.settings.enabled(InlayHintKind::Reification) {
            return inferred;
        }

        // the hint goes where the keyword would be written, which is ahead of
        // everything the parameter's own declaration spells. a parameter that
        // declares a variance cannot take the keyword: a reified one is invariant
        for type_param in type_params {
            if inferred.contains(&type_param.name().id) {
                let writable = match type_param {
                    ast::TypeParam::TypeVar(type_var) => type_var.variance.is_none(),
                    ast::TypeParam::TypeVarTuple(_) | ast::TypeParam::ParamSpec(_) => true,
                };
                self.push(InlayHint::inferred_reification(
                    type_param.range().start(),
                    writable,
                ));
            }
        }
        inferred
    }

    /// Hint the value an enum member takes when its declaration does not write
    /// one, at `position` — after `auto()`, or after a `case` variant's name.
    fn add_enum_member_value(&mut self, name: &str, position: TextSize) {
        if !self.settings.enabled(InlayHintKind::EnumValue) {
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

        self.push(InlayHint::enum_member_value(
            position,
            &rendered.to_string(),
        ));
    }

    /// basedpython: hint `override` on a method that overrides a superclass
    /// member without saying so.
    fn add_inferred_override(&mut self, function: &ast::StmtFunctionDef) {
        let env = &self.model.program_environment();
        if !self.settings.enabled(InlayHintKind::Override) || !self.is_basedpython() {
            return;
        }

        let Some(overridden) = inferred_override(&self.model, function) else {
            return;
        };

        // the range of a `def` excludes its decorators, so this is the modifier
        // position even on a decorated method
        self.push(InlayHint::inferred_override(
            function.range().start(),
            overridden
                .superclass
                .navigation_targets(self.db, env)
                .into_iter()
                .next(),
            &format!("{}.{}", overridden.superclass_name, function.name.id),
        ));
    }

    /// basedpython-ui: hint the observables `function` reads while composing.
    ///
    /// The hint sits after everything the header spells — the return
    /// annotation and the `raises` clause — and before the `:`, so it reads
    /// as one more clause of the header.
    fn add_inferred_reads(&mut self, function: &ast::StmtFunctionDef) {
        if !self.settings.enabled(InlayHintKind::Reads) || !self.is_basedpython() {
            return;
        }

        let Some(reads) = function
            .inferred_type(&self.model)
            .and_then(|ty| inferred_state_reads(self.db, ty))
        else {
            return;
        };

        let position = function
            .raises
            .as_deref()
            .or(function.returns.as_deref())
            .map_or_else(|| function.parameters.end(), Ranged::end);

        self.push(InlayHint::inferred_reads(position, &reads));
    }

    /// basedpython-ui: hint `unstable` before each parameter of the composable
    /// `function` whose declared type the runtime cannot compare.
    fn add_parameter_stability(&mut self, function: &ast::StmtFunctionDef) {
        if !self.settings.enabled(InlayHintKind::Stability) || !self.is_basedpython() {
            return;
        }
        if !function
            .inferred_type(&self.model)
            .is_some_and(|ty| is_composable_function(self.db, ty))
        {
            return;
        }

        let env = &self.model.program_environment();
        for parameter in function.parameters.iter_non_variadic_params() {
            let parameter = &parameter.parameter;
            // an unannotated parameter's type says nothing the annotation did
            // not leave out
            if parameter.annotation.is_none() {
                continue;
            }
            let Some(ty) = hintable_parameter_type(&self.model, parameter) else {
                continue;
            };
            if parameter_stability(self.db, env, ty) == Some(false) {
                self.push(InlayHint::unstable_parameter(
                    parameter.name.range().start(),
                ));
            }
        }
    }

    /// basedpython-ui: hint what a `derived(...)` / `remember(...)` call's
    /// computation depends on, at the end of its line.
    fn add_derived_dependencies(&mut self, call: &ast::ExprCall) {
        if !self.settings.enabled(InlayHintKind::DerivedDeps) || !self.is_basedpython() {
            return;
        }

        let Some(reads) = inferred_derived_dependencies(&self.model, call) else {
            return;
        };

        self.push(InlayHint::derived_dependencies(
            self.source.line_end(call.range().end()),
            &reads,
        ));
    }

    /// basedpython-ui: hint what the observable writes of `site` invalidate.
    /// Only a write made after composing gets one: a write made while
    /// composing is a diagnostic.
    ///
    /// The hint sits at the end of the site itself — after a simple
    /// statement, after a lambda's body — so that two sites on one line
    /// (`a.set(1); b.set(2)`, two lambdas in one call) get one each, in
    /// place. A compound statement's header has no end of its own to sit
    /// after, so a write in one (`if todos.pop():`) is hinted at the end of
    /// the line it is spelled on.
    fn add_invalidations(&mut self, site: WriteSite<'_>) {
        if !self.settings.enabled(InlayHintKind::Invalidates) || !self.is_basedpython() {
            return;
        }

        let Some(invalidations) = inferred_invalidations(&self.model, site) else {
            return;
        };

        let position = match site {
            WriteSite::Statement(stmt) if is_compound_statement(stmt) => {
                self.source.line_end(invalidations.anchor.end())
            }
            WriteSite::Statement(stmt) => stmt.end(),
            WriteSite::Lambda(lambda) => lambda.end(),
        };
        self.push(InlayHint::invalidations(position, &invalidations));
    }

    /// Hint the type arguments inferred for a generic call.
    fn add_call_type_arguments(&mut self, call: &ast::ExprCall, arguments: &[(Name, Type<'db>)]) {
        if !self.settings.enabled(InlayHintKind::CallTypeArguments) || arguments.is_empty() {
            return;
        }

        // an explicit specialization is already written out, and a call whose
        // type arguments all went unsolved says nothing worth reading
        if call.func.is_subscript_expr()
            || arguments.iter().all(|(_, argument)| argument.is_unknown())
        {
            return;
        }

        // a `.py` file runs as written, and only a class has a `__getitem__` to
        // specialize it with there
        let writable = self.is_basedpython()
            || call
                .func
                .inferred_type(&self.model)
                .is_some_and(|callee| matches!(callee, Type::ClassLiteral(_)));
        let context = self.import_context();
        let type_params_in_scope = self.model.type_params_in_scope(call.into());
        self.push(InlayHint::call_type_arguments(
            context,
            call,
            call.func.range().end(),
            arguments,
            self.names_type_arguments(),
            writable,
            &type_params_in_scope,
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
        if !self.settings.enabled(InlayHintKind::ImplicitArgument) || !self.is_basedpython() {
            return;
        }

        let Some(callee) = callee else {
            return;
        };

        let arguments =
            implicit_context_arguments(self.db, env, self.model.file(), callee, call).arguments;
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

        self.push(InlayHint::implicit_context_arguments(
            position,
            last_explicit.is_some(),
            &labelled,
        ));
    }

    /// Hint the name of the type parameter each positional argument of a
    /// subscripted generic fills.
    fn add_type_argument_names(&mut self, subscript: &ast::ExprSubscript) {
        if !self.settings.enabled(InlayHintKind::TypeArgumentName) {
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

        let mut hinted = Vec::new();
        for (name, argument) in names.iter().zip(subscript_arguments(&subscript.slice)) {
            let index = self
                .push(InlayHint::type_argument_name(
                    argument.range().start(),
                    name,
                ))
                .then(|| self.hints.len() - 1);
            hinted.push((argument, Some(name.as_str()), index));
        }

        // basedpython's keyword subscript names a type argument as a call names an argument, and
        // python's subscript has no keyword form
        if self.is_basedpython() {
            self.name_positional_arguments(&hinted);
        }
    }

    /// Hint the extra arms the typing spec's numeric promotion adds to a
    /// `float` / `complex` type expression.
    fn add_numeric_promotion(&mut self, expr: &Expr) {
        let env = &self.model.program_environment();
        if !self.settings.enabled(InlayHintKind::NumericPromotion) || !self.in_type_expression {
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

        self.push(InlayHint::numeric_promotion(expr.range().end(), arms));
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
        if !self.settings.enabled(InlayHintKind::RevealedType) {
            return;
        }

        let Some(argument) = call.arguments.args.iter().exactly_one().ok() else {
            return;
        };
        let Some(revealed) = argument.inferred_type(&self.model) else {
            return;
        };

        self.push(InlayHint::revealed_type(
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
        if !self.settings.enabled(InlayHintKind::ImplicitSelf)
            || !self.is_basedpython()
            || !parameter.range().is_empty()
        {
            return;
        }

        let position = parameter.range().start();
        let ty = hintable_parameter_type(&self.model, parameter);

        self.push(InlayHint::implicit_parameters(
            self.db,
            env,
            InlayHintKind::ImplicitSelf,
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
        if !self.settings.enabled(InlayHintKind::ImplicitParameter) {
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

        self.push(InlayHint::implicit_parameters(
            self.db,
            env,
            InlayHintKind::ImplicitParameter,
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
        if !self.settings.enabled(InlayHintKind::LambdaParameterType)
            || self.lambda.is_none()
            || parameter.annotation.is_some()
            || parameter.range().is_empty()
        {
            return;
        }

        let Some(ty) = hintable_parameter_type(&self.model, parameter) else {
            return;
        };
        let Some(lambda) = self.lambda else {
            return;
        };

        let type_params_in_scope = self.model.type_params_in_scope(lambda.into());
        let mut hint = InlayHint::inferred_annotation(
            self.import_context(),
            InlayHintKind::LambdaParameterType,
            &AnnotationSlot {
                position: parameter.name.range().end(),
                scope_node: lambda.into(),
                default: self.parameter_default,
                parenthesize: lambda.parameters.as_deref().and_then(|parameters| {
                    (self.source.as_bytes().get(parameters.start().to_usize()) != Some(&b'('))
                        .then(|| parameters.range())
                }),
            },
            ": ",
            ty,
            &type_params_in_scope,
            None,
        );
        // only basedpython's parenthesized lambda takes annotations, and a pack's annotation is
        // what each of its elements is rather than the tuple the hint shows
        let is_pack = lambda.parameters.as_deref().is_some_and(|parameters| {
            [parameters.vararg.as_deref(), parameters.kwarg.as_deref()]
                .into_iter()
                .flatten()
                .any(|pack| pack.range() == parameter.range())
        });
        if !self.is_basedpython() || is_pack {
            hint.text_edits.clear();
        }
        self.push(hint);
    }

    /// basedpython: hint the type an unannotated parameter of a `def` takes from
    /// the method it overrides or the overloads it implements.
    ///
    /// A lambda's parameters are typed from the call site instead, and are hinted
    /// under their own setting.
    fn add_inherited_parameter_type(&mut self, parameter: &ast::Parameter) {
        if !self.settings.enabled(InlayHintKind::InheritedParameterType)
            || self.lambda.is_some()
            || parameter.annotation.is_some()
            || parameter.range().is_empty()
            || self.stub_covered.contains(&parameter.name.range().end())
        {
            return;
        }

        let Some(ty) = inherited_parameter_annotation(&self.model, parameter) else {
            return;
        };
        let Some(function) = self.signature_of else {
            return;
        };

        let tooltip =
            inherited_parameter_source(&self.model, parameter).map(|source| match source {
                ParameterTypeSource::Overloads(function) => format!(
                    "Inherited from the overloads of {}",
                    inline_code(function.as_str())
                ),
                ParameterTypeSource::Method(method) => {
                    format!("Inherited from {}", inline_code(&method))
                }
            });
        let type_params_in_scope = self.model.type_params_in_signature(function);
        // a pack's inherited annotation is its elements', as the base writes it, so it is
        // written as it is shown
        self.push(InlayHint::inferred_annotation(
            self.import_context(),
            InlayHintKind::InheritedParameterType,
            &AnnotationSlot {
                position: parameter.name.range().end(),
                scope_node: function.into(),
                default: self.parameter_default,
                parenthesize: None,
            },
            ": ",
            ty,
            &type_params_in_scope,
            tooltip,
        ));
    }

    /// basedpython: hint the type arguments each generic base a class writes
    /// without them takes from the members its body overrides, after the base
    fn add_inferred_base_type_arguments(&mut self, class: &ast::StmtClassDef) {
        if !self
            .settings
            .enabled(InlayHintKind::InferredBaseTypeArguments)
        {
            return;
        }
        for (base, arguments) in inferred_base_type_arguments(&self.model, class) {
            if self.range.intersect(base).is_none() {
                continue;
            }
            let Some(base) = class
                .bases()
                .iter()
                .find(|expression| expression.range() == base)
            else {
                continue;
            };
            let type_params_in_scope = self.model.type_params_in_scope(base.into());
            self.push(InlayHint::inferred_base_type_arguments(
                self.import_context(),
                base,
                &arguments,
                &type_params_in_scope,
            ));
        }
    }

    /// basedpython: hint the bound an unannotated parameter of a `def` takes from
    /// what its body does with it, as the `some` annotation that would declare it
    fn add_inferred_parameter_type(&mut self, parameter: &ast::Parameter) {
        // a `.py` file has no `some` to write the bound with
        if !self.settings.enabled(InlayHintKind::InferredParameterType)
            || !self.is_basedpython()
            || self.lambda.is_some()
            || parameter.annotation.is_some()
            || parameter.range().is_empty()
            || self.stub_covered.contains(&parameter.name.range().end())
        {
            return;
        }

        let Some(bound) = inferred_parameter_bound(&self.model, parameter) else {
            return;
        };
        let Some(function) = self.signature_of else {
            return;
        };

        let type_params_in_scope = self.model.type_params_in_signature(function);
        self.push(InlayHint::inferred_annotation(
            self.import_context(),
            InlayHintKind::InferredParameterType,
            &AnnotationSlot {
                position: parameter.name.range().end(),
                scope_node: function.into(),
                default: self.parameter_default,
                parenthesize: None,
            },
            ": some ",
            bound,
            &type_params_in_scope,
            None,
        ));
    }

    /// basedpython: hint the default a parameter takes from the method its `def`
    /// overrides, where the parameter's own default would be written.
    fn add_inherited_parameter_default(&mut self, parameter: &ast::ParameterWithDefault) {
        if !self.settings.enabled(InlayHintKind::InheritedDefault)
            || self.lambda.is_some()
            || parameter.parameter.range().is_empty()
        {
            return;
        }

        let Some(value) = inherited_parameter_default(&self.model, parameter) else {
            return;
        };

        let inherited_from = self
            .signature_of
            .and_then(|function| inherited_default_source(&self.model, function));
        self.push(InlayHint::inherited_default(
            parameter.parameter.range().end(),
            &value,
            parameter.parameter.annotation.is_some(),
            inherited_from.as_deref(),
        ));
    }

    /// Hint the return type recovered for a `def` that leaves its annotation
    /// out, where the annotation would be written.
    ///
    /// This runs before the `raises` hint, which sits in the same place when
    /// there is no annotation, so the two read in the order they are written:
    /// `def f() -> int raises TypeError`.
    fn add_inferred_return(&mut self, function: &ast::StmtFunctionDef) {
        if !self.settings.enabled(InlayHintKind::InferredReturnType)
            || function.returns.is_some()
            || function.is_asserts_return
            || !self.range.contains_inclusive(function.parameters.end())
            || self.stub_covered.contains(&function.parameters.end())
        {
            return;
        }

        // the return type the signature settled on, whether the annotation's
        // absence left it to return-type inference or to the method it overrides
        let Some(returned) = inferred_return_type(&self.model, function) else {
            return;
        };

        // `None` is what such a `def` already returns, so writing it down says
        // nothing the source did not — `redundant-return-annotation` reports it
        if returned.is_none(self.db) {
            return;
        }

        let context = self.import_context();
        // a predicate's `TypeIs[T]` narrows the first argument a call passes (after the receiver,
        // for a method), so a recovered one is spelled as the guard it stands for, which only
        // basedpython can write. python has no spelling for a guard on any other place
        let predicate = inferred_return_predicate(&self.model, function);
        let predicate = match predicate {
            Some(predicate) if self.is_basedpython() => Some(predicate),
            Some(predicate) if !predicate.is_type_is_parameter => return,
            _ => None,
        };

        let mut type_params_in_scope = self.model.type_params_in_signature(function);
        type_params_in_scope.extend(inferred_parameter_holes(&self.model, function));
        self.push(InlayHint::inferred_return(
            context,
            function,
            predicate.as_ref(),
            returned,
            &type_params_in_scope,
        ));
    }

    /// basedpython: hint the type a property declaration leaves to its accessors
    ///
    /// the construct writes its name once and the getter carries that name's range,
    /// so the type goes where the declaration would have written it — after the
    /// name, exactly as it does for an unannotated variable
    fn add_property_type(&mut self, getter: &ast::StmtFunctionDef) {
        if !self.settings.enabled(InlayHintKind::PropertyType)
            || !self.is_basedpython()
            || getter.property_construct_range().is_none()
            // the declaration named a type; the getter carries it as its return
            || getter.returns.is_some()
            // a malformed accessor recovers to one with no body, which states nothing
            || getter.body.is_empty()
            // the getter is visited for the whole construct, but the hint sits on its name
            || self.range.intersect(getter.name.range()).is_none()
        {
            return;
        }

        let Some(ty) = inferred_property_type(&self.model, getter) else {
            return;
        };

        let type_params_in_scope = self.model.type_params_in_scope(getter.into());
        self.push(InlayHint::inferred_annotation(
            self.import_context(),
            InlayHintKind::PropertyType,
            &AnnotationSlot {
                position: getter.name.range().end(),
                scope_node: getter.into(),
                default: None,
                parenthesize: None,
            },
            ": ",
            ty,
            &type_params_in_scope,
            None,
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
        declaration: Option<&str>,
    ) -> bool {
        if !self.settings.enabled(InlayHintKind::CallArgumentName) {
            return false;
        }

        if name.starts_with('_') {
            return false;
        }

        let inlay_hint =
            InlayHint::call_argument_name(position, name, navigation_target, declaration);

        self.push(inlay_hint)
    }

    /// give each hinted positional argument of one argument list the edit that names it: `x=`
    /// before it, and before every positional argument after it, since one passed by position
    /// cannot follow one passed by name
    ///
    /// `arguments` are the list's positional arguments in source order, each with the name of
    /// the parameter it fills, if it can be passed by one, and the index of its hint, if it has
    /// one. an argument is named only where every argument after it can be: none is unpacked,
    /// which fills its parameters by position and so cannot follow a named one either, and each
    /// fills one parameter that takes a name
    fn name_positional_arguments(&mut self, arguments: &[(&Expr, Option<&str>, Option<usize>)]) {
        let mut names_from_here: Option<Vec<InlayHintTextEdit>> = Some(Vec::new());
        for (argument, name, hint) in arguments.iter().rev() {
            names_from_here = match (names_from_here, name) {
                (Some(mut edits), Some(name)) if !argument.is_starred_expr() => {
                    edits.insert(
                        0,
                        InlayHintTextEdit {
                            range: TextRange::empty(argument.start()),
                            new_text: format!("{name}="),
                        },
                    );
                    Some(edits)
                }
                _ => None,
            };
            if let (Some(edits), Some(hint)) = (&names_from_here, hint) {
                self.hints[*hint].text_edits.clone_from(edits);
            }
        }
    }

    fn import_context(&self) -> InlayHintImportContext<'_, 'db> {
        InlayHintImportContext {
            db: self.db,
            file: self.model.program_file(),
            importer: &self.importer,
            scope_members: Some(&self.scope_members),
        }
    }
}

impl<'a> SourceOrderVisitor<'a> for InlayHintVisitor<'a, '_> {
    fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
        // basedpython: a property getter is ranged onto its accessor, but the
        // construct it was synthesized from reaches back over the declaration —
        // the line the property's own hint sits on. asking about the accessor
        // alone loses that hint whenever the accessor is below the visible range
        let range = match node {
            AnyNodeRef::StmtFunctionDef(function) => function
                .property_construct_range()
                .unwrap_or_else(|| function.range()),
            node => node.range(),
        };

        if self.range.intersect(range).is_some() {
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

        // basedpython-ui: a statement's own writes to state; the statements
        // nested in it are visited below and hinted as sites of their own
        self.add_invalidations(WriteSite::Statement(stmt));

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
                    self.in_trailing_lambda_callee = decorator.expression.is_call_expr();
                    self.visit_decorator(decorator);
                    self.in_trailing_lambda_callee = false;
                }

                let enclosing_class = self.enclosing_class.take();
                self.visit_body(&function.body);
                self.enclosing_class = enclosing_class;

                return;
            }
            // basedpython: a property accessor's whole header is synthesized — the
            // parameter list stands for no source, and the name belongs to the
            // construct's head, which is written once and takes the one hint above
            // — so beyond that only the accessor body is real
            Stmt::FunctionDef(function) if has_synthesized_header(function) => {
                self.add_property_type(function);

                let enclosing_class = self.enclosing_class.take();
                self.visit_body(&function.body);
                self.enclosing_class = enclosing_class;

                return;
            }
            Stmt::FunctionDef(function) => {
                // the stub's annotations first: a slot they fill gets no inferred hint
                self.add_stub_types(function);
                self.add_inferred_return(function);
                self.add_inferred_raises(function);
                self.add_inferred_reads(function);
                self.add_inferred_reification(function);
                self.add_inferred_override(function);
                self.add_parameter_stability(function);

                // a function nested in a method is not itself a class member
                let enclosing_class = self.enclosing_class.take();
                let signature_of = self.signature_of.replace(function);
                source_order::walk_stmt(self, stmt);
                self.signature_of = signature_of;
                self.enclosing_class = enclosing_class;

                return;
            }
            Stmt::ClassDef(class) => {
                // `reified` is written ahead of a variance, so its hint goes first
                let reified = self.add_inferred_class_reification(class);
                self.add_inferred_variances(class.type_params.as_deref(), &reified, |model| {
                    class.inferred_type(model)
                });
                self.add_stub_bases(class);
                self.add_inferred_base_type_arguments(class);

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
                self.add_inferred_variances(type_alias.type_params.as_deref(), &[], |model| {
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

    fn visit_parameter_with_default(&mut self, parameter: &'a ast::ParameterWithDefault) {
        // read, and cleared, by the parameter, which is visited before its default
        self.parameter_default = parameter.default.as_deref();
        source_order::walk_parameter_with_default(self, parameter);
        self.parameter_default = None;

        // after the parameter's own hints: an inherited default is written after the annotation
        // an unannotated parameter's type hint sits at the same place
        if self.enter_node(parameter.into()).is_traverse() {
            self.add_inherited_parameter_default(parameter);
        }
    }

    fn visit_parameter(&mut self, parameter: &'a ast::Parameter) {
        // the default is this parameter's alone: whatever is visited below it, a lambda in an
        // annotation, has parameters of its own
        let default = self.parameter_default.take();
        if self.enter_node(parameter.into()).is_traverse() {
            self.parameter_default = default;
            self.add_implicit_self(parameter);
            self.add_lambda_parameter_type(parameter);
            self.add_inherited_parameter_type(parameter);
            self.add_inferred_parameter_type(parameter);
            self.parameter_default = None;
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
            Expr::Lambda(lambda) => {
                // basedpython-ui: a lambda body runs later, so its writes to
                // state are a site of their own — `on_click=lambda: count.set(0)`
                self.add_invalidations(WriteSite::Lambda(lambda));

                // every parameter below a lambda belongs to a lambda
                let outer = self.lambda.replace(lambda);
                source_order::walk_expr(self, expr);
                self.lambda = outer;
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
                self.add_derived_dependencies(call);

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

                // the block of a trailing lambda is passed after the arguments the call
                // writes, by position, so none of those can be named
                let names_arguments = !std::mem::take(&mut self.in_trailing_lambda_callee);

                self.visit_expr(&call.func);

                let mut positionals = Vec::new();

                // `argument_names` is keyed by positional-arg index, not source-order index,
                // so track them separately to stay in sync after keyword args appear mid-call.
                let mut positional_index = 0;
                for arg_or_keyword in call.arguments.iter_source_order() {
                    if let ArgOrKeyword::Arg(argument) = arg_or_keyword {
                        let parameter = details.argument_names.get(&positional_index);
                        let mut hint = None;
                        if let Some(parameter) = parameter
                            && !arg_matches_name(argument, &parameter.name)
                            && self.add_call_argument_name(
                                arg_or_keyword.range().start(),
                                &parameter.name,
                                parameter.name_range.map(NavigationTarget::from),
                                parameter.declaration.as_deref(),
                            )
                        {
                            hint = Some(self.hints.len() - 1);
                        }
                        positionals.push((
                            argument,
                            parameter.map(|parameter| parameter.name.as_str()),
                            hint,
                        ));

                        positional_index += 1;
                    }

                    self.visit_expr(arg_or_keyword.value());
                }

                if names_arguments {
                    self.name_positional_arguments(&positionals);
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
pub(crate) fn untyped_declaration_value(assign: &ast::StmtAnnAssign) -> Option<&Expr> {
    let Expr::Name(marker) = assign.annotation.as_ref() else {
        return None;
    };
    if !marker.ctx.is_invalid() {
        return None;
    }
    ruff_python_ast::helpers::DeclarationMarker::from_id(marker.id.as_str())
        .is_some_and(|marker| {
            matches!(
                marker.kind,
                ruff_python_ast::helpers::DeclarationMarkerKind::Let
                    | ruff_python_ast::helpers::DeclarationMarkerKind::Assign
            )
        })
        .then_some(assign.value.as_deref())
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
    /// where the members in scope are kept across importers, if they are
    scope_members: Option<&'a ScopeMembers<'db>>,
    /// The node used to compute members in scope (lazily).
    scope_node: AnyNodeRef<'a>,
    scope_offset: TextSize,
    members: Option<MembersInScope<'db>>,
    /// The imports made for every edit applied together with this importer's,
    /// so that each name is imported once among them.
    dynamic_imports: &'a mut FxHashMap<DynamicallyImportedMember, ImportAction>,
    /// The imports this importer added, in the order it added them.
    imported_members: Vec<DynamicallyImportedMember>,
}

impl<'a, 'db> DynamicImporter<'a, 'db> {
    fn new(
        importer: &'a Importer<'db>,
        scope_members: Option<&'a ScopeMembers<'db>>,
        scope_node: AnyNodeRef<'a>,
        scope_offset: TextSize,
        dynamic_imports: &'a mut FxHashMap<DynamicallyImportedMember, ImportAction>,
    ) -> Self {
        Self {
            importer,
            scope_members,
            scope_node,
            scope_offset,
            members: None,
            dynamic_imports,
            imported_members: Vec::new(),
        }
    }

    /// Attempts to import a given symbol.
    /// If the symbol in the text edit needs to be qualified, we return the qualified symbol text.
    ///
    /// basedpython: `implicit` says the file reaches the symbol by its name without importing
    /// it, as a `.by` file does `Iterator`, so that it needs no import unless the file binds the
    /// name to something else.
    #[expect(clippy::too_many_arguments)]
    fn import_symbol(
        &mut self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        ty: &Type<'db>,
        module_name: &str,
        symbol_name: &str,
        label_text: &str,
        implicit: bool,
    ) -> Option<String> {
        use std::collections::hash_map::Entry;

        // Ensure members are computed before borrowing other fields.
        let members = self
            .members
            .get_or_insert_with(|| match self.scope_members {
                Some(scope_members) => {
                    scope_members.at(self.importer, self.scope_node, self.scope_offset)
                }
                None => self
                    .importer
                    .members_in_scope_at(self.scope_node, self.scope_offset),
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
        } else if implicit {
            return None;
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

                // the text the import makes refer to it: the name, or through its module
                // when the import has to qualify it
                Some(action.symbol_text().to_string())
            }
            Entry::Occupied(entry) => Some(entry.get().symbol_text().to_string()),
        }
    }

    /// Imports `symbol_name` from `module_name`, or the module itself when there is no
    /// `symbol_name`, and returns the text that refers to it. A name the file binds already is
    /// taken to be something else, which is written through its module instead.
    fn import_name(&mut self, module_name: &str, symbol_name: Option<&str>) -> String {
        use std::collections::hash_map::Entry;

        let members = self
            .members
            .get_or_insert_with(|| match self.scope_members {
                Some(scope_members) => {
                    scope_members.at(self.importer, self.scope_node, self.scope_offset)
                }
                None => self
                    .importer
                    .members_in_scope_at(self.scope_node, self.scope_offset),
            });
        let key = DynamicallyImportedMember {
            module: module_name.to_string(),
            name: symbol_name.unwrap_or_default().to_string(),
        };
        match self.dynamic_imports.entry(key.clone()) {
            Entry::Vacant(entry) => {
                let request = match symbol_name {
                    None => ImportRequest::module(module_name),
                    Some(name) if members.find_member(name).is_some() => {
                        ImportRequest::import(module_name, name).force()
                    }
                    Some(name) => ImportRequest::import_from(module_name, name),
                };
                let action = entry.insert(self.importer.import(request, members));
                self.imported_members.push(key);
                action.symbol_text().to_string()
            }
            Entry::Occupied(entry) => entry.get().symbol_text().to_string(),
        }
    }

    /// Builds the text edits from all collected imports.
    fn text_edits(&self) -> Vec<InlayHintTextEdit> {
        // placed together, so imports added at one place come out in isort's order
        self.importer
            .combined_edits(
                self.imported_members
                    .iter()
                    .filter_map(|member| self.dynamic_imports.get(member)),
            )
            .into_iter()
            .filter_map(|edit| {
                edit.content().map(|content| InlayHintTextEdit {
                    range: edit.range(),
                    new_text: content.to_string(),
                })
            })
            .collect()
    }
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
            Annotation, Diagnostic, DiagnosticId, DisplayDiagnosticConfig, LintName, Severity,
            Span, SubDiagnostic, SubDiagnosticSeverity,
        },
        files::{File, FileRange, system_path_to_file},
        source::source_text,
    };
    use ruff_diagnostics::{Edit, Fix};
    use ruff_python_parser::parse_unchecked_source;
    use ruff_python_trivia::textwrap::dedent;
    use ruff_text_size::{TextLen, TextSize};
    use rustc_hash::FxHashSet;
    use strum::IntoEnumIterator;

    use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
    use ty_project::ProjectMetadata;
    use ty_project::metadata::options::{AnalysisOptions, Options};
    use ty_python_semantic::ReturnTypeInference;

    fn inlay_hint_test(source: &str) -> InlayHintTest {
        inlay_hint_test_with_options(source, Options::default())
    }

    fn inlay_hint_test_with_options(source: &str, options: Options) -> InlayHintTest {
        inlay_hint_test_in("main.py", source, options)
    }

    /// Like [`inlay_hint_test`], but for a `.by` source, so basedpython-only
    /// hints are produced.
    fn basedpython_inlay_hint_test(source: &str) -> InlayHintTest {
        inlay_hint_test_in("main.by", source, Options::default())
    }

    /// An inlay-hint test with `analysis.sound-types` enabled, for the signatures ty
    /// recovers rather than reads.
    fn sound_types_inlay_hint_test(source: &str) -> InlayHintTest {
        inlay_hint_test_in(
            "main.by",
            source,
            Options {
                analysis: Some(AnalysisOptions {
                    sound_types: Some(true),
                    ..AnalysisOptions::default()
                }),
                ..Options::default()
            },
        )
    }

    fn inlay_hint_test_in(file_name: &str, source: &str, options: Options) -> InlayHintTest {
        const START: &str = "<START>";
        const END: &str = "<END>";

        let mut project = ProjectMetadata::new("test", SystemPathBuf::from("/"));
        project.set_override_options(options);
        let mut db = ty_project::TestDb::new(project);

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

    fn return_type_hint_test(source: &str) -> InlayHintTest {
        inlay_hint_test_with_options(
            source,
            Options {
                analysis: Some(AnalysisOptions {
                    return_type_inference: Some(ReturnTypeInference::Always),
                    ..AnalysisOptions::default()
                }),
                ..Options::default()
            },
        )
    }

    #[test]
    fn inferred_return_types() {
        let mut test = return_type_hint_test(
            r#"
        def unannotated(x: int):
            return str(x)

        def annotated() -> int:
            return 1
        "#,
        );
        assert_snapshot!(test.inlay_hints(), @r#"

        def unannotated(x: int)[ -> str]:
            return str([object=]x)

        def annotated() -> int:
            return 1

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:29
           |
        LL | def unannotated(x: int)[ -> str]:
           |                             ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:22
           |
        LL |     def __new__(cls, object: object = "") -> Self
           |                      ^^^^^^
        info: Source
          --> main2.py:LL:17
           |
        LL |     return str([object=]x)
           |                 ^^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        1 |
          - def unannotated(x: int):
          -     return str(x)
        2 + def unannotated(x: int) -> str:
        3 +     return str(object=x)
        4 |
          |
        "#);

        let mut test = return_type_hint_test(
            r#"
        class C:
            value: str

            def method(self):
                return self.value
        "#,
        );
        assert_snapshot!(test.inlay_hints(), @"

        class C:
            value: str

            def method(self)[ -> str]:
                return self.value

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:26
           |
        LL |     def method(self)[ -> str]:
           |                          ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        4 |
          -     def method(self):
        5 +     def method(self) -> str:
        6 |         return self.value
          |
        ");

        let mut test = return_type_hint_test(
            r#"
        class C:
            value: str

            @property
            def text(self):
                return self.value
        "#,
        );
        assert_snapshot!(test.inlay_hints(), @"

        class C:
            value: str

            @property
            def text(self)[ -> str]:
                return self.value

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:24
           |
        LL |     def text(self)[ -> str]:
           |                        ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        5 |     @property
          -     def text(self):
        6 +     def text(self) -> str:
        7 |         return self.value
          |
        ");

        let mut test = return_type_hint_test(
            r#"
        def stringify(function):
            def wrapper():
                return str(function())

            return wrapper

        @stringify
        def number():
            return 42
        "#,
        );
        assert_snapshot!(test.inlay_hints(), @r#"

        def stringify(function)[ -> () -> str]:
            def wrapper()[ -> str]:
                return str([object=]function())

            return wrapper

        @stringify
        def number()[ -> Literal[42]]:
            return 42

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:35
           |
        LL | def stringify(function)[ -> () -> str]:
           |                                   ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:23
           |
        LL |     def wrapper()[ -> str]:
           |                       ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:22
           |
        LL |     def __new__(cls, object: object = "") -> Self
           |                      ^^^^^^
        info: Source
          --> main2.py:LL:21
           |
        LL |         return str([object=]function())
           |                     ^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:18
           |
        LL | def number()[ -> Literal[42]]:
           |                  ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:26
           |
        LL | def number()[ -> Literal[42]]:
           |                          ^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        1  + from typing import Literal
        2  +
        3  |
        4  | def stringify(function):
           -     def wrapper():
           -         return str(function())
        5  +     def wrapper() -> str:
        6  +         return str(object=function())
        7  |
        8  |     return wrapper
        9  |
        10 | @stringify
           - def number():
        11 + def number() -> Literal[42]:
        12 |     return 42
           |
        "#);

        // basedpython: an overload implementation's unannotated parameter takes the union
        // of what its overloads accept, and its return type the union of what they return
        let mut test = return_type_hint_test(
            r#"
        from typing import overload


        @overload
        def overloaded(x: int) -> int: ...
        @overload
        def overloaded(x: str) -> str: ...
        def overloaded(x):
            return "a" if isinstance(x, str) else 0
        "#,
        );
        assert_snapshot!(test.inlay_hints(), @r#"

        from typing import overload


        @overload
        def overloaded(x: int) -> int: ...
        @overload
        def overloaded(x: str) -> str: ...
        def overloaded(x[: int | str])[ -> int | str]:
            return "a" if isinstance(x, str) else 0

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:20
           |
        LL | def overloaded(x[: int | str])[ -> int | str]:
           |                    ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:26
           |
        LL | def overloaded(x[: int | str])[ -> int | str]:
           |                          ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:36
           |
        LL | def overloaded(x[: int | str])[ -> int | str]:
           |                                    ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:42
           |
        LL | def overloaded(x[: int | str])[ -> int | str]:
           |                                          ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        8  | def overloaded(x: str) -> str: ...
           - def overloaded(x):
        9  + def overloaded(x: int | str) -> int | str:
        10 |     return "a" if isinstance(x, str) else 0
           |
        "#);

        // an `async def`'s annotation names what awaiting it produces, so the hint is `str`
        // rather than the coroutine a call returns
        let mut test = return_type_hint_test(
            r#"
        async def asynchronous(x: int):
            return f"{x}"
        "#,
        );
        assert_snapshot!(test.inlay_hints(), @r#"

        async def asynchronous(x: int)[ -> str]:
            return f"{x}"

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:36
           |
        LL | async def asynchronous(x: int)[ -> str]:
           |                                    ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        1 |
          - async def asynchronous(x: int):
        2 + async def asynchronous(x: int) -> str:
        3 |     return f"{x}"
          |
        "#);
    }

    #[test]
    fn return_type_hint_imports() {
        let mut test = return_type_hint_test(
            r#"
        from module import make

        def create():
            return make()
        "#,
        );
        test.with_extra_file(
            "module.py",
            &dedent(
                r#"
        class Item: ...

        def make() -> Item: ...
        "#,
            ),
        );
        assert_snapshot!(test.inlay_hints(), @"

        from module import make

        def create()[ -> Item]:
            return make()

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> module.py:2:7
          |
        2 | class Item: ...
          |       ^^^^
        info: Source
         --> main2.py:4:18
          |
        4 | def create()[ -> Item]:
          |                  ^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        1 |
          - from module import make
        2 + from module import Item, make
        3 |
          - def create():
        4 + def create() -> Item:
        5 |     return make()
          |
        ");
    }

    /// `source` with `edits` applied, which may be in any order but do not overlap
    fn apply_text_edits(source: &str, edits: &[InlayHintTextEdit]) -> String {
        let mut edited = source.to_string();
        // from the end back, so each edit's range is still where it was
        for edit in edits.iter().sorted_by_key(|edit| edit.range.start()).rev() {
            edited.replace_range(edit.range.to_std_range(), &edit.new_text);
        }
        edited
    }

    struct InlayHintTest {
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
            // a `.by` file is read as basedpython, whose syntax several edits write
            let source_type = self.file.source_type(&self.db);
            let source_has_errors =
                parse_unchecked_source(&text_edit_buf, source_type).has_invalid_syntax();

            // each hint is applied on its own, so each one's edits must make valid source
            // alone, whatever the others write
            if !source_has_errors {
                for hint in &hints {
                    let edited = apply_text_edits(&text_edit_buf, &hint.text_edits);
                    let parsed = parse_unchecked_source(&edited, source_type);
                    assert!(
                        !parsed.has_invalid_syntax(),
                        "the edits of the hint `{}` alone make a syntax error:\n{}\n{edited}",
                        hint.display(),
                        parsed.errors().iter().map(|error| &error.error).join("\n"),
                    );
                }
            }

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

            // Hints can be applied independently, but these snapshots combine all edits.
            // Deduplicate imports shared by multiple hints before applying them together.
            let all_edits: Vec<_> = all_edits
                .into_iter()
                .unique_by(|edit| (edit.range, edit.new_text.clone()))
                .collect();
            let mut edit_offset = TextSize::default();

            for edit in all_edits.iter().sorted_by_key(|edit| edit.range.start()) {
                let updated_range = edit.range + edit_offset;
                text_edit_buf.replace_range(updated_range.to_std_range(), &edit.new_text);
                edit_offset += edit.new_text.text_len() - edit.range.len();
            }

            let edited = parse_unchecked_source(&text_edit_buf, source_type);
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

        /// every hint that writes itself, applied alone, as a client applies one: the label,
        /// then each line the edited file has that the source does not
        ///
        /// asserts that what each writes is what it says: the edited file parses where the
        /// source does, checks with no diagnostic the source did not have, and no longer shows
        /// the hint, since the source now says it
        fn edits_applied_alone(&mut self) -> String {
            let settings = InlayHintSettings::default();
            let path = self
                .file
                .path(&self.db)
                .as_system_path()
                .expect("a test file is a system file")
                .to_path_buf();
            let source = source_text(&self.db, self.file).as_str().to_string();
            let source_type = self.file.source_type(&self.db);
            let whole = TextRange::up_to(source.text_len());

            let hints = self.hints_with(whole, &settings);
            let diagnostics = self.diagnostic_messages();
            let shown = |hints: &[InlayHint], hint: &InlayHint| {
                hints
                    .iter()
                    .filter(|other| {
                        other.kind == hint.kind
                            && other.display().to_string() == hint.display().to_string()
                    })
                    .count()
            };

            let mut rendered = Vec::new();
            for hint in hints.iter().filter(|hint| !hint.text_edits.is_empty()) {
                let edited = apply_text_edits(&source, &hint.text_edits);
                let label = hint.display().to_string();

                let parsed = parse_unchecked_source(&edited, source_type);
                assert!(
                    !parsed.has_invalid_syntax(),
                    "`{label}` writes a syntax error:\n{edited}"
                );

                self.db.write_file(&path, &edited).unwrap();
                let introduced: Vec<_> = self
                    .diagnostic_messages()
                    .into_iter()
                    .filter(|message| !diagnostics.contains(message))
                    .collect();
                assert!(
                    introduced.is_empty(),
                    "`{label}` writes source that checks with {introduced:#?}:\n{edited}"
                );
                let after = self.hints_with(TextRange::up_to(edited.text_len()), &settings);
                assert!(
                    shown(&after, hint) < shown(&hints, hint),
                    "`{label}` is still shown once written:\n{edited}"
                );

                let added = edited
                    .lines()
                    .filter(|line| !source.lines().any(|original| original == *line))
                    .map(|line| format!("  {line}"))
                    .join("\n");
                rendered.push(format!("«{label}» {:?}\n{added}", hint.kind));
            }
            self.db.write_file(&path, &source).unwrap();
            rendered.join("\n")
        }

        /// every hint's tooltip, and every label part's, after its label
        fn tooltips(&mut self) -> String {
            let hints = self.hints_with(self.range, &InlayHintSettings::default());
            hints
                .iter()
                .flat_map(|hint| {
                    let label = hint.display().to_string();
                    let parts = hint.label.parts().iter().filter_map(move |part| {
                        Some(format!("«{}» part: {}", part.text(), part.tooltip()?))
                    });
                    hint.tooltip
                        .as_ref()
                        .map(|tooltip| format!("«{label}»: {tooltip}"))
                        .into_iter()
                        .chain(parts)
                        .collect::<Vec<_>>()
                })
                .join("\n")
        }

        fn hints_with(&self, range: TextRange, settings: &InlayHintSettings) -> Vec<InlayHint> {
            inlay_hints(
                &self.db,
                ProgramFile::new(
                    &self.db,
                    self.file,
                    self.db.program_environment().program(&self.db),
                ),
                range,
                settings,
            )
        }

        /// what the file's check reports, by rule and message, without where
        fn diagnostic_messages(&self) -> Vec<String> {
            ty_project::check_file_ignoring_check_mode(&self.db, self.file)
                .iter()
                .map(|diagnostic| format!("{}: {}", diagnostic.id(), diagnostic.concise_message()))
                .collect()
        }

        fn render_diagnostic<D>(&self, diagnostic: D) -> String
        where
            D: IntoDiagnostic,
        {
            use std::fmt::Write;

            let mut buf = String::new();

            let config = DisplayDiagnosticConfig::new("ty").color(false).context(0);

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
        let mut test = basedpython_ui_inlay_hint_test(
            r#"
            from basedpython_ui import composable, derived, state, Text

            class Base:
                def f(self) -> None: ...

            class Derived(Base):
                def f(self):
                    raise TypeError

            class Source[T]:
                def get(self) -> T: ...

            def make[T](t: T):
                print(T)
                return t

            @composable
            def Counter(items: list[int]):
                let count = state(0)
                let doubled = derived(lambda: count.value * 2)
                Text(f"{doubled.value}")
            "#,
        );

        assert_snapshot!(test.padded_hints(&InlayHintSettings {
            inferred_return_types: true,
            inferred_raises: true,
            inferred_variance: true,
            inferred_reification: true,
            inferred_override: true,
            inferred_reads: true,
            parameter_stability: true,
            derived_dependencies: true,
            ..InlayHintSettings::none()
        }), @"
        «override_»
        «_raises TypeError»
        «out_»
        «reified_»
        «_-> T»
        «unstable_»
        «_reads doubled»
        «_depends on count»
        ");
    }

    /// every kind of hint is shown with only its own setting on, and nothing
    /// else is. a hint is only ever added when its kind's setting is on, so this
    /// is what catches a hint whose work is skipped by the wrong setting before
    /// it is ever made
    #[test]
    fn every_kind_is_switched_by_the_setting_it_names() {
        fn only(kind: InlayHintKind) -> InlayHintSettings {
            let mut settings = InlayHintSettings::none();
            settings.set_enabled(kind, true);
            settings
        }

        let mut by = basedpython_ui_inlay_hint_test(
            r#"
            from basedpython_ui import composable, derived, state, Button, Text

            def identity[T](x: T) -> T:
                return x

            def two(a: int, b: str) -> None: ...

            x = identity(1)
            two(1, 'b')
            y = map(lambda v: v + 1, [1])
            z: dict[str, int] = {}

            def raiser():
                raise TypeError

            class Box[T]:
                def get(self) -> T: ...

            def reify[T]():
                print(T)

            class Base:
                def f(self, a: int = 1) -> None: ...

            class Child(Base):
                def f(self, a) -> None: ...

            def reads(a):
                _ = a.foo

            class Held[T]:
                x: T

            class HeldInt(Held):
                x = 1

            reveal_type(x)

            def apply(fn: (int) -> None) -> None:
                fn(1)

            apply:
                print(it)

            class Init:
                init(a: int)

            class Prop:
                let a
                    get() = 1

            def ctx(context c: int) -> None: ...
            context cc = 1
            ctx()

            enum class Color:
                case Red, Green

            @composable
            def Counter(items: list[int]):
                let count = state(0)
                let doubled = derived(lambda: count.value * 2)
                Text(f"{doubled.value}")
                Button("+"):
                    count.value += 1
            "#,
        );
        let mut python = inlay_hint_test(
            "
            def f(x: float | None) -> None:
                reveal_type(x)
            ",
        );

        // a module with a stub, for the hints it takes from the stub
        let mut stubbed =
            inlay_hint_test_in("stubbed.py", "def g(a):\n    pass\n", Options::default());
        stubbed.with_extra_file(
            "stubbed.pyi",
            "def g(a: int) -> None: ...\ndef h() -> None: ...\n",
        );

        let mut produced = FxHashSet::default();
        for kind in InlayHintKind::iter() {
            let settings = only(kind);
            for test in [&mut by, &mut python, &mut stubbed] {
                let hints = inlay_hints(
                    &test.db,
                    ProgramFile::new(
                        &test.db,
                        test.file,
                        test.db.program_environment().program(&test.db),
                    ),
                    test.range,
                    &settings,
                );
                for hint in hints {
                    assert_eq!(
                        hint.kind,
                        kind,
                        "`{}` produced a {:?} hint",
                        kind.setting(),
                        hint.kind,
                    );
                    produced.insert(kind);
                }
            }
        }

        let missing: Vec<_> = InlayHintKind::iter()
            .filter(|kind| !produced.contains(kind))
            .map(InlayHintKind::setting)
            .collect();
        assert!(missing.is_empty(), "no source produced {missing:?}");
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

        def deco[Parameters: (*: *, **: *), R](fn: (**Parameters) -> R)[ -> (**Parameters) -> R]:
            def inner(*args: *Parameters, **kwargs: **Parameters) -> R:
                positional[: *Parameters] = args
                keyword[: **Parameters] = kwargs
                return fn(*args, **kwargs)

            return inner

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.by:2:37
          |
        2 | def deco[Parameters: (*: *, **: *), R](fn: (**Parameters) -> R):
          |                                     ^
        info: Source
         --> main2.py:2:87
          |
        2 | def deco[Parameters: (*: *, **: *), R](fn: (**Parameters) -> R)[ -> (**Parameters) -> R]:
          |                                                                                       ^
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

    /// a property declaration that names no type takes one from its initialiser, or
    /// from what its getter returns when it has none, and it is shown where the
    /// declaration would have written it
    #[test]
    fn property_type_hints() {
        let mut test = basedpython_inlay_hint_test(
            "
            class A:
                let a
                    get() = 1

                let b
                    field = 'two'

                var c = 0
                    get() = field
                    set(value): field = value

                let d: bytes
                    get() = b''
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            property_types: true,
            ..InlayHintSettings::none()
        }));
    }

    /// the declaration a property hint sits on is a line above the accessor the
    /// parser ranged the getter onto, so a request that stops short of the
    /// accessor still has to reach it
    #[test]
    fn property_type_hint_above_the_requested_range() {
        let mut test = basedpython_inlay_hint_test(
            "
            class A:
            <START>    let a<END>
                    get() = 1
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            property_types: true,
            ..InlayHintSettings::none()
        }));
    }

    /// a request covering only the accessor leaves out the declaration the hint
    /// sits on
    #[test]
    fn property_type_hint_outside_the_requested_range() {
        let mut test = basedpython_inlay_hint_test(
            "
            class A:
                let a
            <START>        get() = 1<END>
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            property_types: true,
            ..InlayHintSettings::none()
        }));
    }

    /// a `private` property's getter is named `_b` while the source spells `b`, and
    /// a `static let` is a class-level descriptor rather than a `property`, but both
    /// are hinted after the name the declaration wrote. a getter that falls off its
    /// end returns `None`, which is the property's type like any other
    #[test]
    fn property_type_hint_shapes() {
        let mut test = basedpython_inlay_hint_test(
            "
            class A:
                private let b
                    get() = 'b'

                static let c
                    get() = 3

                let n
                    get(): pass
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            property_types: true,
            ..InlayHintSettings::none()
        }));
    }

    /// a type ty could not settle on says nothing a reader could write: a getter
    /// that reads its own property never settles, and a malformed accessor recovers
    /// to one with no body
    #[test]
    fn property_type_hint_without_a_settled_type() {
        let mut test = basedpython_inlay_hint_test(
            "
            class A:
                let rec
                    get() = self.rec

                let m
                    get()
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            property_types: true,
            ..InlayHintSettings::none()
        }));
    }

    /// accessor blocks are basedpython syntax: a python file reports them as invalid
    /// and hints nothing about them
    #[test]
    fn property_type_hint_in_a_python_file() {
        let mut test = inlay_hint_test(
            "
            class A:
                let a
                    get() = 1
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            property_types: true,
            ..InlayHintSettings::none()
        }));
    }

    #[test]
    fn property_type_hint_can_be_turned_off() {
        let mut test = basedpython_inlay_hint_test(
            "
            class A:
                let a
                    get() = 1
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            property_types: false,
            ..InlayHintSettings::default()
        }));
    }

    /// a function whose every return wraps another call to itself never settles on
    /// a return type, so there is nothing to hint. (one that calls itself before it
    /// can return at all, `return f()`, never returns, and is hinted `-> Never`)
    #[test]
    fn inferred_return_type_that_never_settles() {
        let mut test = basedpython_inlay_hint_test(
            "
            def f(n: int):
                return [f(n) for _ in range(n)]
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_return_types: true,
            ..InlayHintSettings::none()
        }));
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
        2  +
        3  |
        --------------------------------------------------------------------------------
        7  | x = 1
           - y = x
           - z = i(1)
           - w = z
        8  + y: Literal[1] = x
        9  + z: int = i(1)
        10 + w: int = z
        11 | aa = b'foo'
           - bb = aa
        12 + bb: Literal[b"foo"] = aa
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

    /// python has no spelling for a repeated tuple, so a hint in a python file shows the type it
    /// reduces to, which that file can hold as an annotation
    #[test]
    fn test_repeated_tuple_hint_is_reduced_in_python() {
        let mut test = inlay_hint_test(
            "
            def f(n: int):
                pairs = (1, 'a') * n
            ",
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        def f(n: int):
            pairs[: tuple[Literal[1, "a"], ...]] = (1, 'a') * n

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class tuple[out Element](Sequence[Element]):
           |       ^^^^^
        info: Source
          --> main2.py:LL:13
           |
        LL |     pairs[: tuple[Literal[1, "a"], ...]] = (1, 'a') * n
           |             ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:19
           |
        LL |     pairs[: tuple[Literal[1, "a"], ...]] = (1, 'a') * n
           |                   ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:27
           |
        LL |     pairs[: tuple[Literal[1, "a"], ...]] = (1, 'a') * n
           |                           ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:30
           |
        LL |     pairs[: tuple[Literal[1, "a"], ...]] = (1, 'a') * n
           |                              ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        1 + from typing import Literal
        2 +
        3 |
        4 | def f(n: int):
          -     pairs = (1, 'a') * n
        5 +     pairs: tuple[Literal[1, "a"], ...] = (1, 'a') * n
          |
        "#);
    }

    #[test]
    fn test_repeated_tuple_hint_is_insertable_in_basedpython() {
        let mut test = basedpython_inlay_hint_test(
            "
            def f(n: int):
                pairs = (1, 'a') * n
            ",
        );

        assert_snapshot!(test.inlay_hints(), @r#"

        def f(n: int):
            pairs[: (1, "a") * int] = (1, 'a') * n

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class tuple[out Element](Sequence[Element]):
           |       ^^^^^
        info: Source
          --> main2.py:LL:13
           |
        LL |     pairs[: (1, "a") * int] = (1, 'a') * n
           |             ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:14
           |
        LL |     pairs[: (1, "a") * int] = (1, 'a') * n
           |              ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:17
           |
        LL |     pairs[: (1, "a") * int] = (1, 'a') * n
           |                 ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:24
           |
        LL |     pairs[: (1, "a") * int] = (1, 'a') * n
           |                        ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.by:1:1
          |
        2 | def f(n: int):
          -     pairs = (1, 'a') * n
        3 +     pairs: (1, "a") * int = (1, 'a') * n
          |
        "#);
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
        2  +
        3  |
        --------------------------------------------------------------------------------
        9  | x = (1, 'abc')
           - y = x
           - z = (i(1), s('abc'))
           - w = z
        10 + y: tuple[Literal[1], Literal["abc"]] = x
        11 + z: tuple[int, str] = (i(1), s('abc'))
        12 + w: tuple[int, str] = z
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
        1  + from typing import Literal
        2  +
        3  |
        --------------------------------------------------------------------------------
        7  | x: int = 1
           - y = x
        8  + y: Literal[1] = x
        9  | z: int = i(1)
           - w = z
        10 + w: int = z
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
                self.y[: y] = y

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
        2 +
        3 |
        4 | def my_func(command: str):
        5 |     match command.split():
        6 |         case ["get", ("a" | "b") as ab]:
          -             x = ab
        7 +             x: Literal["a", "b"] = ab
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
        1  + from string.templatelib import Template
        2  |
           - a = [1, 2]
           - b = [1.0, 2.0]
           - c = [True, False]
        3  + a: list[int] = [1, 2]
        4  + b: list[int | float] = [1.0, 2.0]
        5  + c: list[bool] = [True, False]
        6  | d = [None, None]
           - e = ["hel", "lo"]
           - f = ['the', 're']
           - g = [f"{ft}", f"{ft}"]
           - h = [t"wow %d", t"wow %d"]
           - i = [b'/x01', b'/x02']
           - j = [+1, +2.0]
           - k = [-1, -2.0]
        7  + e: list[str] = ["hel", "lo"]
        8  + f: list[str] = ['the', 're']
        9  + g: list[str] = [f"{ft}", f"{ft}"]
        10 + h: list[Template] = [t"wow %d", t"wow %d"]
        11 + i: list[bytes] = [b'/x01', b'/x02']
        12 + j: list[int | float] = [+1, +2.0]
        13 + k: list[int | float] = [-1, -2.0]
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

    /// basedpython reads an upper-case name a class body assigns as `final`, and
    /// an `auto()` member written that way is hinted all the same
    #[test]
    fn basedpython_enum_auto_values() {
        let mut test = basedpython_inlay_hint_test(
            r#"
            from enum import IntEnum, auto

            class Color(IntEnum):
                RED = auto()
                GREEN = auto()
            "#,
        );

        assert_snapshot!(test.inlay_hints(), @"

        from enum import IntEnum, auto

        class Color(IntEnum):
            RED = auto()[ 1]
            GREEN = auto()[ 2]
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
                self.x[: list[T]] = x
                self.y[: tuple[U, U]] = y

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
        LL |         self.x[: list[T]] = x
           |                  ^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:15
          |
        2 | class MyClass[T, U]:
          |               ^
        info: Source
         --> main2.py:4:23
          |
        4 |         self.x[: list[T]] = x
          |                       ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class tuple[out Element](Sequence[Element]):
           |       ^^^^^
        info: Source
          --> main2.py:LL:18
           |
        LL |         self.y[: tuple[U, U]] = y
           |                  ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:18
          |
        2 | class MyClass[T, U]:
          |                  ^
        info: Source
         --> main2.py:5:24
          |
        5 |         self.y[: tuple[U, U]] = y
          |                        ^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:18
          |
        2 | class MyClass[T, U]:
          |                  ^
        info: Source
         --> main2.py:5:27
          |
        5 |         self.y[: tuple[U, U]] = y
          |                           ^

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
        3  |     def __init__(self, x: list[T], y: tuple[U, U]):
           -         self.x = x
           -         self.y = y
        4  +         self.x: list[T] = x
        5  +         self.y: tuple[U, U] = y
        6  |
           - x = MyClass([42], ("a", "b"))
           - y = (MyClass([42], ("a", "b")), MyClass([42], ("a", "b")))
           - a, b = MyClass([42], ("a", "b")), MyClass([42], ("a", "b"))
           - c, d = (MyClass([42], ("a", "b")), MyClass([42], ("a", "b")))
        7  + x: MyClass[int, str] = MyClass[int, str](x=[42], y=("a", "b"))
        8  + y: tuple[MyClass[int, str], MyClass[int, str]] = (MyClass[int, str](x=[42], y=("a", "b")), MyClass[int, str](x=[42], y=("a", "b")))
        9  + a, b = MyClass[int, str](x=[42], y=("a", "b")), MyClass[int, str](x=[42], y=("a", "b"))
        10 + c, d = (MyClass[int, str](x=[42], y=("a", "b")), MyClass[int, str](x=[42], y=("a", "b")))
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
        8 + Foo(x=1, y='a')
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
        3 + foo(x=1, y='hello', z=True)
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
        5 + total: int = add(x=3, b=2, y=4)
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
        4 + foo(x=1, y='custom')
        5 + foo(x=1, y='custom', z=True)
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
        10 + baz(a=foo(x=5), b=bar(y=bar(y='test')), c=True)
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
        5 + bar(a=1, b=2)
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
        2  +
        3  |
        --------------------------------------------------------------------------------
        14 |         x = None
           -     y = x
        15 +     y: Literal[1, 2, 3, "hello"] | None = x
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
        3 + foo(1, 'pos', c=3.14, d=False, e=42)
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
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:13
           |
        LL | def foo(x[: int | str])[ -> str | int]:
           |             ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:19
           |
        LL | def foo(x[: int | str])[ -> str | int]:
           |                   ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:29
           |
        LL | def foo(x[: int | str])[ -> str | int]:
           |                             ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:35
           |
        LL | def foo(x[: int | str])[ -> str | int]:
           |                                   ^^^

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
        7  | def foo(x: str) -> int: ...
           - def foo(x):
        8  + def foo(x: int | str) -> str | int:
        9  |     return x
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
          --> main2.py:LL:13
           |
        LL | def S()[ -> Sequence[str]]:
           |             ^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:22
           |
        LL | def S()[ -> Sequence[str]]:
           |                      ^^^

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
        7  | def S(*names: str, is_symmetric: Optional[bool] = None) -> Sequence[str]: ...
           - def S():
        8  + def S() -> Sequence[str]:
        9  |     pass
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
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:11
           |
        LL | def f(x[: int | str])[ -> str | int]:
           |           ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:17
           |
        LL | def f(x[: int | str])[ -> str | int]:
           |                 ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:27
           |
        LL | def f(x[: int | str])[ -> str | int]:
           |                           ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:33
           |
        LL | def f(x[: int | str])[ -> str | int]:
           |                                 ^^^

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
        7  | def f(x: str, y: str) -> int: ...
           - def f(x):
        8  + def f(x: int | str) -> str | int:
        9  |     return x
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
        7 + foo(x=1, y=2)
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

        a[: def foo(x: int, *y: bool, z: str | int | list[str])] = foo
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:16
           |
        LL | a[: def foo(x: int, *y: bool, z: str | int | list[str])] = foo
           |                ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:13
           |
        LL | final class bool(int):
           |             ^^^^
        info: Source
          --> main2.py:LL:25
           |
        LL | a[: def foo(x: int, *y: bool, z: str | int | list[str])] = foo
           |                         ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:34
           |
        LL | a[: def foo(x: int, *y: bool, z: str | int | list[str])] = foo
           |                                  ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:40
           |
        LL | a[: def foo(x: int, *y: bool, z: str | int | list[str])] = foo
           |                                        ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:46
           |
        LL | a[: def foo(x: int, *y: bool, z: str | int | list[str])] = foo
           |                                              ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:51
           |
        LL | a[: def foo(x: int, *y: bool, z: str | int | list[str])] = foo
           |                                                   ^^^
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
        4 + N = NewType(name='N', tp=str)
        5 |
          |
        ");
    }

    #[test]
    fn typevar_in_scope_inlay_hint() {
        let mut test = inlay_hint_test(
            "
        def f[T](t: T):
            a = t",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def f[T](t: T):
            a[: T] = t
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | def f[T](t: T):
          |       ^
        info: Source
         --> main2.py:3:9
          |
        3 |     a[: T] = t
          |         ^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | def f[T](t: T):
          -     a = t
        3 +     a: T = t
          |
        ");
    }

    #[test]
    fn hidden_typevar_inlay_hint() {
        let mut test = inlay_hint_test(
            "
        def f[T](t: T):
            def g[T](u: T):
                a = t",
        );

        assert_snapshot!(test.inlay_hints(), @"

        def f[T](t: T):
            def g[T](u: T):
                a[: T] = t
        ");
    }

    /// a type parameter's name means another one here, or something else altogether, so no
    /// annotation can say it, on its own or inside another type
    #[test]
    fn a_hidden_typevar_offers_no_annotation() {
        for source in [
            "def f[T](t: T):\n    def g[T](u: T):\n        a = t\n",
            "def f[T](t: T):\n    def g[T](u: T):\n        a = [t]\n",
            "class C[T]:\n    def m(self, t: T):\n        def g[T](u: T):\n            a = t\n",
            "def f[T](t: T):\n    T = 1\n    a = t\n",
        ] {
            let test = inlay_hint_test(source);
            let hints = inlay_hints(
                &test.db,
                ProgramFile::new(
                    &test.db,
                    test.file,
                    test.db.program_environment().program(&test.db),
                ),
                test.range,
                &InlayHintSettings::default(),
            );
            let annotations: Vec<_> = hints
                .iter()
                .flat_map(|hint| &hint.text_edits)
                .map(|edit| edit.new_text.as_str())
                .filter(|edit| edit.starts_with(": "))
                .collect();
            assert_eq!(annotations, Vec::<&str>::new(), "{source}");
        }
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
            y[: type[T]] = x
        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class type:
           |       ^^^^
        info: Source
          --> main2.py:LL:9
           |
        LL |     y[: type[T]] = x
           |         ^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:7
          |
        2 | def f[T](x: type[T]):
          |       ^
        info: Source
         --> main2.py:3:14
          |
        3 |     y[: type[T]] = x
          |              ^
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
        LL | Protocol: type[_Protocol]
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
          --> stdlib/typing.byi:LL:22
           |
        LL |                 cls, name: str, value: dynamic, *, type_params: (*: _TypeParameter) = (), qualname: str? = None
           |                      ^^^^
        info: Source
          --> main2.py:LL:20
           |
        LL | A = TypeAliasType([name=]'A', [value=]str)
           |                    ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing.byi:LL:33
           |
        LL |                 cls, name: str, value: dynamic, *, type_params: (*: _TypeParameter) = (), qualname: str? = None
           |                                 ^^^^^
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
        3 + A = TypeAliasType(name='A', value=str)
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
          --> stdlib/typing.byi:LL:17
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

    /// basedpython: an unannotated parameter's inferred type parameter is written by the
    /// parameter's name, and a return type mentioning one is offered as no annotation
    #[test]
    fn inferred_return_leaves_out_the_scope_of_a_type_variable() {
        let mut test = return_type_hint_test(
            r#"
        class A:
            def f(self):
                return self
        "#,
        );
        assert!(test.inlay_hints().contains("def f(self)[ -> Self]:"));
    }

    #[test]
    fn inferred_return_names_an_inferred_parameter_by_its_parameter() {
        let mut test = return_type_hint_test(
            r#"
        def twice(value):
            return [value, value]
        "#,
        );
        assert_snapshot!(test.inlay_hints(), @"

        def twice(value)[ -> list[value]]:
            return [value, value]

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:22
           |
        LL | def twice(value)[ -> list[value]]:
           |                      ^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.py:2:11
          |
        2 | def twice(value):
          |           ^^^^^
        info: Source
         --> main2.py:2:27
          |
        2 | def twice(value)[ -> list[value]]:
          |                           ^^^^^
        ");
    }

    #[test]
    fn inferred_unknown_type() {
        // we do not suggest edits/imports for ty-internal types (Unknown), even if they are nested
        let mut test = return_type_hint_test(
            r#"
        def firsts(values: list):
            return [values[0]]
        "#,
        );
        assert_snapshot!(test.inlay_hints(), @"

        def firsts(values: list)[ -> list[Unknown]]:
            return [values[0]]

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class list[in out Element](MutableSequence[Element]):
           |       ^^^^
        info: Source
          --> main2.py:LL:30
           |
        LL | def firsts(values: list)[ -> list[Unknown]]:
           |                              ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/ty_extensions/_internal.pyi:LL:1
           |
        LL | Unknown: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:35
           |
        LL | def firsts(values: list)[ -> list[Unknown]]:
           |                                   ^^^^^^^
        ");

        // We still suggest edits/imports for a user-defined class named Unknown.
        let mut test = return_type_hint_test(
            r#"
        from module import make_unknown

        def custom_unknown():
            return make_unknown()
        "#,
        );
        test.with_extra_file(
            "module.py",
            &dedent(
                r#"
        class Unknown: ...

        def make_unknown() -> Unknown:
            return Unknown()
        "#,
            ),
        );
        assert_snapshot!(test.inlay_hints(), @"

        from module import make_unknown

        def custom_unknown()[ -> Unknown]:
            return make_unknown()

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> module.py:2:7
          |
        2 | class Unknown: ...
          |       ^^^^^^^
        info: Source
         --> main2.py:4:26
          |
        4 | def custom_unknown()[ -> Unknown]:
          |                          ^^^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        1 |
          - from module import make_unknown
        2 + from module import Unknown, make_unknown
        3 |
          - def custom_unknown():
        4 + def custom_unknown() -> Unknown:
        5 |     return make_unknown()
          |
        ");
    }

    #[test]
    fn typeshed_helper_type() {
        // We do not suggest edits/imports for typeshed-internal types (PathEntryFinderProtocol).
        let mut test = inlay_hint_test(
            r#"
        import pkgutil

        finder = pkgutil.get_importer(path_item=".")
        "#,
        );
        assert_snapshot!(test.inlay_hints(), @r#"

        import pkgutil

        finder[: PathEntryFinderProtocol | None] = pkgutil.get_importer(path_item=".")

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/_typeshed/importlib.byi:LL:10
           |
        LL | protocol PathEntryFinderProtocol:
           |          ^^^^^^^^^^^^^^^^^^^^^^^
        info: Source
          --> main2.py:LL:10
           |
        LL | finder[: PathEntryFinderProtocol | None] = pkgutil.get_importer(path_item=".")
           |          ^^^^^^^^^^^^^^^^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/types.byi:LL:13
           |
        LL | final class NoneType:
           |             ^^^^^^^^
        info: Source
          --> main2.py:LL:36
           |
        LL | finder[: PathEntryFinderProtocol | None] = pkgutil.get_importer(path_item=".")
           |                                    ^^^^
        "#);
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
        2 | import foo
        3 + from bar import D
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
        1 |
          - from foo import C
        2 + from bar import D
        3 + from foo import A, B, C
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
        6 + a: D[Baz] = D[Baz](x=Baz)
          |
        ");
    }

    #[test]
    fn auto_import_each_hint_independently() {
        // Each hint needs its own imports, even when another hint uses the same types.
        // Repeated types within a hint should only produce one import per symbol.
        let mut test =
            inlay_hint_test("from module import y\nx = (y, 1, y, 1)\nx = (y, 1, y, 1)\n");
        test.with_extra_file(
            "module.py",
            &dedent(
                r#"
        class Y: ...
        y: Y
        "#,
            ),
        );
        let hints = inlay_hints(
            &test.db,
            ProgramFile::new(
                &test.db,
                test.file,
                test.db.program_environment().program(&test.db),
            ),
            test.range,
            &InlayHintSettings::default(),
        );

        assert_eq!(hints.len(), 2);
        let source = source_text(&test.db, test.file);
        let edited_sources = hints
            .into_iter()
            .map(|hint| {
                let mut edited = source.as_str().to_string();
                for edit in hint
                    .text_edits
                    .iter()
                    .sorted_by_key(|edit| edit.range.start())
                    .rev()
                {
                    edited.replace_range(edit.range.to_std_range(), &edit.new_text);
                }
                edited
            })
            .join("\n");

        assert_snapshot!(edited_sources, @"
        from typing import Literal

        from module import Y, y
        x: tuple[Y, Literal[1], Y, Literal[1]] = (y, 1, y, 1)
        x = (y, 1, y, 1)

        from typing import Literal

        from module import Y, y
        x = (y, 1, y, 1)
        x: tuple[Y, Literal[1], Y, Literal[1]] = (y, 1, y, 1)
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
          --> stdlib/typing.byi:LL:1
           |
        LL | Literal: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:43
           |
        LL |     a[: Any | Literal["some"]] = getattr[[Literal["some"]]](x, 'foo', "some")
           |                                           ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class str(Sequence[str]):
           |       ^^^
        info: Source
          --> main2.py:LL:51
           |
        LL |     a[: Any | Literal["some"]] = getattr[[Literal["some"]]](x, 'foo', "some")
           |                                                   ^^^^^^

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
        1 |
        2 + from typing import Any, TypeVar
        3 +
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
        1 |
        2 + import bar
        3 + import baz
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
        1 |
        2 + import bar
        3 + import baz
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
        11 + b: B[foo.A] = B[foo.A](x=foo.A())
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
    fn basedpython_inferred_raises_of_a_generic_callee() {
        let mut test = basedpython_inlay_hint_test(
            "
            def rethrow[T: BaseException](error: T) raises T:
                raise error

            def caller():
                rethrow(TypeError())
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
            context let d = foo()
            final e = foo()
            let f: int = foo()
            var g: int = foo()
            let h
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
    fn basedpython_inferred_reification_of_a_forwarded_type_parameter() {
        let mut test = basedpython_inlay_hint_test(
            "
            class Box[T]:
                def __init__(self, t: T):
                    self.t: T = t

            def forwarded[T](t: T) -> object:
                return Box(t)

            def hop[U](u: U) -> object:
                return forwarded(u)

            def erased[V](v: V) -> list[V]:
                return [v]
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_reification: true,
            ..InlayHintSettings::none()
        }));
    }

    #[test]
    fn basedpython_inferred_class_reification() {
        let mut test = basedpython_inlay_hint_test(
            "
            class ValueUse[T]:
                def kind(self):
                    return T

            class AnnotationOnly[T]:
                value: T

            class Declared[reified T]:
                pass

            class HalfDeclared[reified T, U]:
                def kind(self):
                    return U
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
    fn basedpython_inferred_override_of_a_private_member() {
        let mut test = basedpython_inlay_hint_test(
            "
            class A:
                private def f(self) -> None: ...

            class B(A):
                private def f(self) -> None: ...
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_override: true,
            ..InlayHintSettings::none()
        }));
    }

    /// The mock of the `basedpython_ui` framework the ui hints are tested
    /// against: its observables, slot functions and the builders the examples
    /// use, developed in place as a first-party package — which is how the
    /// framework itself is developed.
    const BASEDPYTHON_UI_INIT: &str = "
from .modifier export Modifier, Alignment, Arrangement, TextStyle, NONE, DEFAULT_TEXT_STYLE
from .runtime export (
    State,
    StateList,
    StateDict,
    Derived,
    Indexed,
    state,
    state_list,
    state_dict,
    derived,
    remember,
    composable,
    Ambient,
    ambient,
    provide,
    Job,
    launched_effect,
)
from .widgets export ColumnScope, RowScope, Text, Button, TextField, Checkbox, Column, Row
from .app export run_app, compose_test, TestComposition
";

    const BASEDPYTHON_UI_MODIFIER: &str = r##"
enum class Alignment:
    case Start, Center, End

enum class Arrangement:
    case Start, Center, End

frozen data class TextStyle:
    color: str = "#202020"
    size: float = 14.0

frozen data class Modifier:
    ops: tuple[int, ...] = ()

    def padding(self, all: int | float) -> Modifier: ...

let NONE = Modifier()
let DEFAULT_TEXT_STYLE = TextStyle()
"##;

    const BASEDPYTHON_UI_RUNTIME: &str = "
from collections.abc import Iterable, Iterator

class State[T]:
    value: T
    def __init__(self, initial: T) -> None: ...
    def set(self, new: T) -> None: ...
    def update(self, fn: (T) -> T) -> None: ...

frozen data class Indexed[T]:
    index: int
    item: T

class StateList[T]:
    def __init__(self, initial: Iterable[T] = ()) -> None: ...
    def __len__(self) -> int: ...
    def __iter__(self) -> Iterator[T]: ...
    def __getitem__(self, index: int) -> T: ...
    def __setitem__(self, index: int, value: T) -> None: ...
    def append(self, value: T) -> None: ...
    def insert(self, index: int, value: T) -> None: ...
    def remove_at(self, index: int) -> None: ...
    def remove(self, value: T) -> None: ...
    def pop(self, index: int = -1) -> T: ...
    def clear(self) -> None: ...
    def index_where(self, predicate: (T) -> bool) -> int?: ...
    def snapshot(self) -> tuple[T, ...]: ...
    def each(self, key: (T) -> object, local content: (T) -> None) -> None: ...
    def each_indexed(self, key: (T) -> object, local content: (Indexed[T]) -> None) -> None: ...

class StateDict[K, V]:
    def __len__(self) -> int: ...
    def __contains__(self, key: K) -> bool: ...
    def __getitem__(self, key: K) -> V: ...
    def __setitem__(self, key: K, value: V) -> None: ...
    def get(self, key: K) -> V | None: ...
    def remove(self, key: K) -> None: ...
    def keys(self) -> tuple[K, ...]: ...
    def items(self) -> tuple[tuple[K, V], ...]: ...

class Derived[T]:
    value: T

class Ambient[T]:
    current: T

class Job: ...

class Runtime:
    def set_root(self, root: () -> None) -> None: ...

def state[T](initial: T) -> State[T]: ...
def state_list[T](initial: Iterable[T] = ()) -> StateList[T]: ...
def state_dict[K, V]() -> StateDict[K, V]: ...
def derived[T](compute: () -> T) -> Derived[T]: ...
def remember[T](compute: () -> T) -> T: ...
def composable[F](fn: F) -> F: ...
def ambient[T](default: T) -> Ambient[T]: ...
def provide[T](which: Ambient[T], value: T, once content: () -> None) -> None: ...
def launched_effect(key: object, block: (Job) -> None) -> None: ...
def builder[F](fn: F) -> F: ...
";

    const BASEDPYTHON_UI_WIDGETS: &str = r#"
from .modifier import Modifier, Alignment, Arrangement, TextStyle, NONE, DEFAULT_TEXT_STYLE
from .runtime import builder

class ColumnScope:
    def weight(self, value: float) -> Modifier: ...
    def align(self, alignment: Alignment) -> Modifier: ...

class RowScope:
    def weight(self, value: float) -> Modifier: ...
    def align(self, alignment: Alignment) -> Modifier: ...

@builder
def Text(text: str, modifier: Modifier = NONE, style: TextStyle = DEFAULT_TEXT_STYLE) -> None: ...
@builder
def Button(label: str, modifier: Modifier = NONE, enabled: bool = True, on_click: () -> None) -> None: ...
@builder
def TextField(value: str, modifier: Modifier = NONE, placeholder: str = "", on_change: (str) -> None) -> None: ...
@builder
def Checkbox(checked: bool, modifier: Modifier = NONE, on_change: (bool) -> None) -> None: ...
@builder
def Column(modifier: Modifier = NONE, arrangement: Arrangement = Arrangement.Start, once content: ColumnScope.() -> None) -> None: ...
@builder
def Row(modifier: Modifier = NONE, arrangement: Arrangement = Arrangement.Start, once content: RowScope.() -> None) -> None: ...
"#;

    const BASEDPYTHON_UI_APP: &str = r#"
class TestComposition: ...

def run_app(title: str = "basedpython-ui", width: int = 800, height: int = 600, root: () -> None) -> None: ...
def compose_test(width: int = 400, height: int = 300, root: () -> None) -> TestComposition: ...
"#;

    /// Like [`basedpython_inlay_hint_test`], with the mock `basedpython_ui`
    /// package installed beside the source.
    fn basedpython_ui_inlay_hint_test(source: &str) -> InlayHintTest {
        let mut test = basedpython_inlay_hint_test(source);
        test.with_extra_file("basedpython_ui/__init__.by", BASEDPYTHON_UI_INIT);
        test.with_extra_file("basedpython_ui/modifier.by", BASEDPYTHON_UI_MODIFIER);
        test.with_extra_file("basedpython_ui/runtime.by", BASEDPYTHON_UI_RUNTIME);
        test.with_extra_file("basedpython_ui/widgets.by", BASEDPYTHON_UI_WIDGETS);
        test.with_extra_file("basedpython_ui/app.by", BASEDPYTHON_UI_APP);
        test
    }

    /// The four basedpython-ui hints, and nothing else.
    fn basedpython_ui_settings() -> InlayHintSettings {
        InlayHintSettings {
            inferred_reads: true,
            parameter_stability: true,
            derived_dependencies: true,
            inferred_invalidations: true,
            ..InlayHintSettings::none()
        }
    }

    /// The observables a function reads while composing: `.value` on a state
    /// or a derived, `.current` on an ambient, iteration / `len` / a subscript
    /// / `in` / `each` on a collection, and a `context` parameter — in the
    /// body and its content blocks, never in a handler, a lambda or a nested
    /// `def`. A callee's reads come through its parameters and globals, its
    /// own cells stay its own, and a callee that cannot be followed is `…` —
    /// which on its own is worth saying of a composable, and of nothing else.
    #[test]
    fn basedpython_inferred_reads() {
        let mut test = basedpython_ui_inlay_hint_test(
            r#"
            from basedpython_ui import State, StateDict, StateList, ambient, composable, derived, state, Button, Column, Text

            let THEME = ambient("light")

            class Model:
                count: State[int]

                def total(self) -> int:
                    return self.count.value

            def label(cell: State[int]) -> str:
                return f"{cell.value}"

            def loud(cell: State[int]) -> str:
                let local = state(0)
                return label(cell) + str(local.value)

            @composable
            def Counter(step: int = 1):
                let count = state(0)
                Column:
                    Text(loud(count))
                    Button("+"):
                        count.value += step

            @composable
            def Dashboard(model: Model, items: StateList[str], table: StateDict[str, int], context depth: int):
                let total = derived(lambda: model.total())
                Text(f"{total.value} {model.total()} {len(items)} {THEME.current} {depth}")
                for item in items:
                    Text(item)
                if "a" in table:
                    Text(str(table["a"]))
                items.each(key=lambda item: item):
                    Text(it)

                def later():
                    Text(str(items[0]))

                Button("x", on_click=lambda: model.count.set(0))

            def opaque(cell: State[int], thing: dynamic) -> int:
                thing()
                return cell.value

            def entry(thing: dynamic):
                thing()

            @composable
            def Blind(thing: dynamic):
                thing()

            def declared(cell: State[int]) -> int raises ValueError:
                return cell.value

            def nothing(): ...
            "#,
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_reads: true,
            ..InlayHintSettings::none()
        }));
    }

    /// `unstable` goes before a composable parameter whose declared type the
    /// runtime cannot compare; a stable one, an unannotated one and a plain
    /// function's parameters get nothing.
    #[test]
    fn basedpython_parameter_stability() {
        let mut test = basedpython_ui_inlay_hint_test(
            "
            from basedpython_ui import State, StateList, composable

            frozen data class Todo:
                title: str

            data class Draft:
                title: str

            @composable
            def TodoList(
                items: list[str],
                view: list[out str],
                todos: StateList[Todo],
                draft: Draft,
                todo: Todo,
                count: State[int],
                on_click: () -> None,
                untyped,
                table: dict[str, int] = {},
            ): ...

            def helper(items: list[str]): ...
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            parameter_stability: true,
            ..InlayHintSettings::none()
        }));
    }

    /// What a `derived` or `remember` computation depends on is shown at the
    /// end of its line: the observables its lambda reads, its callees
    /// followed. A computation that reads nothing gets no hint.
    #[test]
    fn basedpython_derived_dependencies() {
        let mut test = basedpython_ui_inlay_hint_test(
            r#"
            from basedpython_ui import State, composable, derived, remember, state, state_list

            def validate(value: str) -> str?:
                return None if value else "required"

            def total_of(cell: State[int]) -> int:
                return cell.value

            @composable
            def Form():
                let name = state("")
                let email = state("")
                let count = state(0)
                let items = state_list([1])
                let name_error = derived(lambda: validate(name.value))
                let full = derived(lambda: name.value + email.value)
                let total = derived(lambda: total_of(count) + sum(1 for item in items if item))
                let cached = remember(lambda: count.value * 2)
                let constant = derived(lambda: 1)
            "#,
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            derived_dependencies: true,
            ..InlayHintSettings::none()
        }));
    }

    /// What a state write made after composing invalidates: the composables
    /// whose own composition reads the place — through the body, its content
    /// blocks and the plain functions it calls, and through a composable
    /// called with a content block, which the runtime runs inline in its
    /// parent — the `derived` values whose lambda reads it and then their
    /// readers, a child handed the slot (directly, through a plain helper's
    /// parameter, from another module) when it reads its parameter, the
    /// `root` of an entry point. A slot declared in a content block is its
    /// composable's; a name bound to another place is followed to it, in the
    /// body and in a handler. Callers, inline parents and readers of a
    /// module-level slot in other files cannot be followed, nor can a
    /// `dynamic` callee, an unpacked argument, or a written name that is not
    /// a slot — a loop target, a value bound outside composition — so those
    /// sets end in `…`. A write nobody observes says `nothing`; a write made
    /// while composing, and a write to something that is not an observable,
    /// get no hint. Each site is hinted at its own end, so two statements on
    /// one line get one each.
    #[test]
    fn basedpython_invalidations() {
        let mut test = basedpython_ui_inlay_hint_test(
            r#"
            from basedpython_ui import (
                Derived, State, StateDict, ambient, composable, compose_test, derived, launched_effect, remember,
                state, state_dict, state_list, Button, Column, Text,
            )
            from basedpython_ui.runtime import Runtime
            from remote import Remote

            let THEME = ambient("light")
            let CLICKS = State(0)


            class Model:
                count: State[int]

                def total(self) -> int:
                    return self.count.value

                def reset(self):
                    self.count.set(0)


            @composable
            def Child(count: State[int]):
                Text(str(count.value))


            @composable
            def Forwarding(count: State[int]):
                Child(count)


            @composable
            def Display(total: Derived[int]):
                Text(str(total.value))


            @composable
            def Counter(step: int = 1):
                let count = state(0)
                let seed = state(1)
                let unread = state(0)
                let total = derived(lambda: count.value * 2)
                let cached = remember(lambda: seed.value)
                let todos = state_list([1])
                let table: StateDict[str, int] = state_dict()
                let plain = [1]
                Column:
                    Text(f"{count.value} {total.value} {cached} {len(todos)} {table.get('a')}")
                    Forwarding(count)
                    Display(total)
                    Button("+"):
                        count.value += step
                    Button("reset", on_click=lambda: count.set(0))
                    Button("todo"):
                        todos.append(2)
                        table["a"] = 1
                    Button("both"):
                        count.value += step; todos.append(3)
                    Button("seed"):
                        seed.value = 2
                    Button("unread"):
                        unread.set(1)
                    Button("plain"):
                        plain.append(1)
                launched_effect(None):
                    count.value = 5

                def later():
                    count.value = 0


            @composable
            def Outer():
                let shared = state(0)

                @composable
                def Inner():
                    Text(str(shared.value))

                Inner()
                Button("bump"):
                    shared.value += 1


            @composable
            def OuterInline():
                let shared = state(0)

                @composable
                def Inner(once content: () -> None):
                    Text(str(shared.value))
                    content()

                Inner():
                    Text("x")
                Button("bump"):
                    shared.value += 1


            @composable
            def Themed():
                Text(THEME.current)
                Text(str(CLICKS.value))
                Button("click", on_click=lambda: CLICKS.set(1))


            @composable
            def Other():
                Text(str(CLICKS.value))


            def reset_clicks():
                CLICKS.set(0)


            @composable
            def Editor(count: State[int]):
                Text(str(count.value))
                Button("+"):
                    count.value += 1


            @composable
            def Host():
                let count = state(0)
                Text(str(count.value))
                Editor(count)
                Remote(count)
                Button("+"):
                    count.value += 1


            @composable
            def Blind(thing: dynamic):
                let cell = state(0)
                thing(cell)
                Button("x"):
                    cell.set(1)


            @composable
            def Spread():
                let count = state(0)
                let cells = (count,)
                Child(*cells)
                Button("x"):
                    count.set(1)


            @composable
            def Broken():
                let count = state(0)
                count.value = 1
                Text(str(count.value))


            @composable
            def Dashboard(model: Model):
                Text(str(model.total()))
                Button("reset", on_click=lambda: model.count.set(0))
                Button("local"):
                    let cell = model.count
                    cell.set(2)


            @composable
            def Card(count: State[int], once content: () -> None):
                let expanded = state(False)
                Text(f"{count.value} {expanded.value}")
                content()
                Button("expand"):
                    expanded.set(True)
                Button("zero"):
                    count.set(0)


            @composable
            def Inline():
                let count = state(0)
                Card(count):
                    Text("inner")
                Child(count)
                Button("+"):
                    count.value += 1


            @composable
            private def Mid(count: State[int], once content: () -> None):
                let seen = state(False)
                Text(str(seen.value))
                Card(count):
                    content()
                Button("seen"):
                    seen.set(True)


            @composable
            def Top():
                let count = state(0)
                let flag = state(False)
                Text(str(flag.value))
                Mid(count):
                    Text("deep")
                Button("+"):
                    count.value += 1
                Button("flag"):
                    flag.set(True)


            @composable
            def InBlock():
                Column:
                    let inner = state(0)
                    let twice = derived(lambda: inner.value * 2)
                    Text(f"{inner.value} {twice.value}")
                    Child(inner)
                    Button("+"):
                        inner.value += 1


            @composable
            def WriterAlias():
                let count = state(0)
                Text(str(count.value))
                Button("alias"):
                    let alias = count
                    alias.set(5)
                Button("loop"):
                    for c in [count]:
                        c.set(6)
                Button("comp"):
                    _ = [c.set(7) for c in [count]]


            @composable
            def ReaderAlias():
                let count = state(0)
                let alias = count
                Text(str(alias.value))
                Button("+"):
                    count.value += 1


            private def render(cell: State[int]):
                Child(cell)


            @composable
            def ViaHelper():
                let count = state(0)
                render(count)

                def captured():
                    Editor(count)

                captured()
                Button("+"):
                    count.value += 1


            def install(rt: Runtime):
                def root():
                    let c = state(0)
                    Text(str(c.value))
                    Button("+"):
                        c.value += 1
                rt.set_root(root)


            def install_lambda(rt: Runtime):
                let cell = State(0)
                rt.set_root(lambda: Child(cell))

                def bump():
                    cell.set(1)


            def test_root():
                let t = compose_test:
                    let count = state(0)
                    Text(str(count.value))
                    Button("+"):
                        count.value += 1
            "#,
        );
        test.with_extra_file(
            "remote.by",
            r#"
from basedpython_ui import State, composable, Text


@composable
def Remote(cell: State[int]):
    Text(f"remote {cell.value}")
"#,
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_invalidations: true,
            ..InlayHintSettings::none()
        }));
    }

    /// `examples/counter.by` of basedpython-ui, verbatim.
    #[test]
    fn basedpython_ui_counter_example() {
        let mut test = basedpython_ui_inlay_hint_test(
            r#"
            from basedpython_ui import composable, state, Column, Row, Text, Button, Modifier, Alignment, run_app


            @composable
            def Counter(step: int = 1):
                let count = state(0)                        # State[int], remembered for this scope's lifetime
                Column(Modifier().padding(16)):
                    Text(f"count = {count.value}")          # a tracked read: this scope now depends on `count`
                    Row:
                        Button("-", enabled=count.value > 0):
                            count.value -= step             # the block binds `on_click`; a write invalidates readers
                        Button("+"):
                            count.value += step
                        Button("reset", enabled=count.value != 0):
                            count.value = 0
                    if count.value > 9:
                        Text("that is a lot", modifier=align(Alignment.Center))   # `align` comes from the ColumnScope receiver


            @composable
            def App():
                Column:
                    Counter()
                    Counter(step=5)


            def main():
                run_app("counter"):
                    App()
            "#,
        );

        assert_snapshot!(test.inlay_hints_with_settings(&basedpython_ui_settings()));
    }

    /// `examples/todo.by` of basedpython-ui, verbatim.
    #[test]
    fn basedpython_ui_todo_example() {
        let mut test = basedpython_ui_inlay_hint_test(
            r#"
            from basedpython_ui import (
                composable, state, state_list, derived, Column, Row, Text, Button, Checkbox, TextField, Modifier, run_app,
            )


            frozen data class Todo:            # immutable: the only way to change a todo is to replace it in the list
                id: int
                title: str
                done: bool = False


            @composable
            def TodoRow(todo: Todo, on_delete: () -> None, on_toggle: (bool) -> None):
                Row(Modifier().padding(4)):
                    Checkbox(todo.done, on_change=on_toggle)
                    Text(todo.title, modifier=weight(1.0))
                    Button("x", on_click=on_delete)


            @composable
            def TodoApp():
                let todos = state_list([Todo(1, "write the runtime"), Todo(2, "ship it")])   # StateList[Todo]: observable
                let draft = state("")
                let next_id = state(3)
                let remaining = derived(lambda: sum(1 for t in todos if not t.done))        # ⟨depends on todos⟩

                Column(Modifier().padding(12)):
                    Text(f"{remaining.value} of {len(todos)} remaining")
                    Row:
                        TextField(draft.value, placeholder="what needs doing?", modifier=weight(1.0)):
                            draft.value = it                                                # `it: str`, the new text
                        Button("add", enabled=draft.value != ""):
                            todos.append(Todo(next_id.value, draft.value))
                            next_id.value += 1
                            draft.value = ""
                    todos.each_indexed(key=lambda todo: todo.id):                          # keyed children; `it` is this row
                        let row = it                                                        # the inner block below has its own `it`
                        TodoRow(row.item, on_delete=lambda: todos.remove_at(row.index)):
                            todos[row.index] = Todo(row.item.id, row.item.title, it)        # `it: bool` from `on_toggle`
                    if len(todos) == 0:
                        Text("nothing to do")


            def main():
                run_app("todos"):
                    TodoApp()
            "#,
        );

        assert_snapshot!(test.inlay_hints_with_settings(&basedpython_ui_settings()));
    }

    /// `examples/form.by` of basedpython-ui, verbatim.
    #[test]
    fn basedpython_ui_form_example() {
        let mut test = basedpython_ui_inlay_hint_test(
            r##"
            from basedpython_ui import composable, state, derived, Column, Row, Text, Button, TextField, Modifier, TextStyle, run_app


            let ERROR_STYLE = TextStyle(color="#b00020")


            frozen data class Signup:
                name: str
                email: str


            def validate_name(value: str) -> str?:
                return "name is required" if value.strip() == "" else None


            def validate_email(value: str) -> str?:
                if "@" not in value or value.startswith("@"):
                    return "enter a valid e-mail address"
                return None


            @composable
            def Field(label: str, value: str, error: str?, on_change: (str) -> None):
                Column(Modifier().padding(4)):
                    Text(label)
                    TextField(value, on_change=on_change)
                    if error is not None:
                        Text(error, style=ERROR_STYLE)


            @composable
            def SignupForm(on_submit: (Signup) -> None):
                let name = state("")
                let email = state("")
                let submitted = state(False)
                # derived values are recomputed only when a dependency changes, and never observed half-updated
                let name_error = derived(lambda: validate_name(name.value))
                let email_error = derived(lambda: validate_email(email.value))
                let can_submit = derived(lambda: name_error.value is None and email_error.value is None)

                Column(Modifier().padding(16)):
                    Field("name", name.value, name_error.value if submitted.value else None):
                        name.value = it
                    Field("e-mail", email.value, email_error.value if submitted.value else None):
                        email.value = it
                    Row:
                        Button("sign up", enabled=can_submit.value or not submitted.value):
                            submitted.value = True
                            if can_submit.value:
                                on_submit(Signup(name.value, email.value))
                        Button("clear"):
                            name.value = ""
                            email.value = ""
                            submitted.value = False


            def main():
                run_app("sign up"):
                    SignupForm(on_submit=lambda s: print("welcome", s.name, s.email))
            "##,
        );

        assert_snapshot!(test.inlay_hints_with_settings(&basedpython_ui_settings()));
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

    /// The same, for a callee reached through an import. The semantic index binds
    /// the block's `it` before anything resolves across modules, so it assumes one
    /// rather than losing one — the hint has the callee's type and must not.
    #[test]
    fn basedpython_implicit_parameters_without_an_argument_across_modules() {
        let mut test = basedpython_inlay_hint_test(
            "
            from callees import apply, each

            apply:
                print('hi')

            each:
                print(it)
            ",
        );
        test.with_extra_file(
            "callees.by",
            "
def apply(fn: () -> None) -> None:
    fn()

def each(fn: (int) -> None) -> None:
    fn(1)
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

    /// with `typeArgumentNames` off, the types other hints show leave their type
    /// arguments unnamed, while those hints are still shown
    #[test]
    fn basedpython_type_arguments_unnamed_with_the_setting_off() {
        let mut test = basedpython_inlay_hint_test(
            "
            class Pair[Key, Value]:
                init(key: Key, value: Value)

            a = Pair(1, 'x')
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            variable_types: true,
            call_type_arguments: true,
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
    /// than the missing annotation did. A base that leaves its return type out
    /// has a receiver of its own `Self`, which is no reason to pass nothing on.
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

            class D:
                def f(self, a: int, *args: str): ...

            class E(D):
                def f(self, a, *args): ...
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inherited_parameter_types: true,
            ..InlayHintSettings::none()
        }));
    }

    /// A method's defaults are part of what it declares, so an override that leaves one out
    /// keeps it — and the hint shows the value the call site will bind, where the override
    /// would have written it. A parameter the base never defaulted has nothing to show, and
    /// neither has one whose default is an expression rather than a value.
    #[test]
    fn basedpython_inherited_parameter_defaults() {
        let mut test = basedpython_inlay_hint_test(
            "
            class A:
                def f(self, a=1, b: str = 'x', c=None, d=[], e=2) -> None: ...

            class B(A):
                def f(self, a, b: str, c, d, e=3) -> None: ...

            class C:
                def f(self, a) -> None: ...
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inherited_parameter_defaults: true,
            ..InlayHintSettings::none()
        }));
    }

    /// An override that repeats `_` takes the names its base gives those positions, so a
    /// call is hinted with the base's names. The numbered `_` of a function that overrides
    /// nothing is positional-only, and hinted with nothing.
    #[test]
    fn basedpython_repeated_underscore_parameter_names() {
        let mut test = basedpython_inlay_hint_test(
            "
            class A:
                def f(self, x: int = 1, y: str = 'a') -> None: ...

            class B(A):
                override def f(self, _, _) -> None: ...

            def g(_: int, _: int) -> None: ...

            B().f(1, 'b')
            g(1, 2)
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            call_argument_names: true,
            inherited_parameter_defaults: true,
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

    /// a generic base written without type arguments takes them from the
    /// members the class body overrides, and they are shown where they would be
    /// written. a base written with its arguments, and one the body does not
    /// solve, get no hint
    #[test]
    fn basedpython_inferred_base_type_arguments() {
        let mut test = basedpython_inlay_hint_test(
            "
            class A[T]:
                x: T

            class B(A):
                x = 1

            class C(A[object]):
                x = 1

            class D(A):
                y = 1
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_base_type_arguments: true,
            ..InlayHintSettings::none()
        }));
    }

    /// an unannotated parameter is bounded by what its body does with it, and
    /// the bound is shown as the `some` annotation that would declare it. a
    /// parameter the body asks nothing of, an annotated one and a method's
    /// receiver get no hint
    #[test]
    fn basedpython_inferred_parameter_types() {
        let mut test = basedpython_inlay_hint_test(
            "
            def read(a):
                _ = a.foo

            def called(a, b=1):
                x: int = a.size()
                return b

            def untouched(a):
                pass

            def annotated(a: int):
                _ = a.real

            class C:
                def m(self, a):
                    _ = a.foo
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_parameter_types: true,
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

    fn stub_settings() -> InlayHintSettings {
        InlayHintSettings {
            stub_types: true,
            stub_only_members: true,
            ..InlayHintSettings::none()
        }
    }

    impl InlayHintTest {
        /// every whole-line hint, as `above 3:` or `below 3:` (the line the hint is drawn on the
        /// side of) and its lines, and its kind and edits on the line after
        fn block_hints(&self, settings: &InlayHintSettings) -> String {
            let index = ruff_db::source::line_index(&self.db, self.file);
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
            .filter_map(|hint| {
                let placement = hint.placement?;
                let line = index.line_index(hint.position).get();
                let label = hint
                    .label
                    .parts()
                    .iter()
                    .map(InlayHintLabelPart::text)
                    .join("");
                let target = hint
                    .label
                    .parts()
                    .iter()
                    .filter_map(|part| part.target())
                    .map(|target| {
                        let text = source_text(&self.db, target.file());
                        format!(
                            "{}@{:?}",
                            target.file().path(&self.db),
                            text[target.focus_range()]
                                .lines()
                                .next()
                                .unwrap_or_default()
                        )
                    })
                    .join(", ");
                Some(format!(
                    "{} {line}: ({}) -> {target}\n{label}",
                    placement.as_str(),
                    hint.kind.setting()
                ))
            })
            .join("\n")
        }
    }

    #[test]
    fn stub_types_fill_what_a_runtime_def_leaves_out() {
        let mut test = inlay_hint_test(
            "
            class Parser:
                def parse(self, text, strict=False):
                    return text.split()

            def helper(a, *args, **kwargs):
                pass

            def annotated(a: int) -> int:
                return a
            ",
        );
        test.with_extra_file(
            "main.pyi",
            "class Parser:\n    def parse(self, text: str, strict: bool = ...) -> list[str]: ...\n\
             def helper(a: int, *args: str, **kwargs: object) -> None: ...\n\
             def annotated(a: str) -> str: ...\n",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&stub_settings()), @"

        class Parser:
            def parse(self, text[: str], strict[: bool]=False)[ -> list[str]]:
                return text.split()

        def helper(a[: int], *args[: str], **kwargs[: object])[ -> None]:
            pass

        def annotated(a: int) -> int:
            return a

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.pyi:2:27
          |
        2 |     def parse(self, text: str, strict: bool = ...) -> list[str]: ...
          |                           ^^^
        info: Source
         --> main2.py:3:28
          |
        3 |     def parse(self, text[: str], strict[: bool]=False)[ -> list[str]]:
          |                            ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.pyi:2:40
          |
        2 |     def parse(self, text: str, strict: bool = ...) -> list[str]: ...
          |                                        ^^^^
        info: Source
         --> main2.py:3:43
          |
        3 |     def parse(self, text[: str], strict[: bool]=False)[ -> list[str]]:
          |                                           ^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.pyi:2:55
          |
        2 |     def parse(self, text: str, strict: bool = ...) -> list[str]: ...
          |                                                       ^^^^^^^^^
        info: Source
         --> main2.py:3:60
          |
        3 |     def parse(self, text[: str], strict[: bool]=False)[ -> list[str]]:
          |                                                            ^^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.pyi:3:15
          |
        3 | def helper(a: int, *args: str, **kwargs: object) -> None: ...
          |               ^^^
        info: Source
         --> main2.py:6:16
          |
        6 | def helper(a[: int], *args[: str], **kwargs[: object])[ -> None]:
          |                ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.pyi:3:27
          |
        3 | def helper(a: int, *args: str, **kwargs: object) -> None: ...
          |                           ^^^
        info: Source
         --> main2.py:6:30
          |
        6 | def helper(a[: int], *args[: str], **kwargs[: object])[ -> None]:
          |                              ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.pyi:3:42
          |
        3 | def helper(a: int, *args: str, **kwargs: object) -> None: ...
          |                                          ^^^^^^
        info: Source
         --> main2.py:6:47
          |
        6 | def helper(a[: int], *args[: str], **kwargs[: object])[ -> None]:
          |                                               ^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.pyi:3:53
          |
        3 | def helper(a: int, *args: str, **kwargs: object) -> None: ...
          |                                                     ^^^^
        info: Source
         --> main2.py:6:60
          |
        6 | def helper(a[: int], *args[: str], **kwargs[: object])[ -> None]:
          |                                                            ^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        2 | class Parser:
          -     def parse(self, text, strict=False):
        3 +     def parse(self, text: str, strict: bool = False) -> list[str]:
        4 |         return text.split()
        5 |
          - def helper(a, *args, **kwargs):
        6 + def helper(a: int, *args: str, **kwargs: object) -> None:
        7 |     pass
          |
        ");
    }

    #[test]
    fn stub_types_of_overloads_are_only_what_every_overload_says() {
        let mut test = inlay_hint_test(
            "
            def get(key, default=None):
                return default
            ",
        );
        test.with_extra_file(
            "main.pyi",
            "from typing import overload\n\
             @overload\ndef get(key: str) -> str | None: ...\n\
             @overload\ndef get(key: str, default: int) -> str | int: ...\n",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&stub_settings()), @"

        def get(key[: str], default=None):
            return default

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.pyi:3:14
          |
        3 | def get(key: str) -> str | None: ...
          |              ^^^
        info: Source
         --> main2.py:2:15
          |
        2 | def get(key[: str], default=None):
          |               ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        1 |
          - def get(key, default=None):
        2 + def get(key: str, default=None):
        3 |     return default
          |
        ");
    }

    /// the hints in a runtime module's signature say what the checker reads it as, and the
    /// checker reads it as the stub declares it: a slot the stub's overloads agree on takes the
    /// stub's own words, one they disagree on the union the checker reads (the hint of a
    /// parameter type taken from elsewhere), one the stub types with its own class that class as
    /// the runtime module's, and one the stub types with a class the module lacks nothing
    #[test]
    fn stub_types_say_what_the_checker_reads() {
        let mut test = inlay_hint_test(
            "
            class Node: ...

            def get(key, default=0):
                reveal_type(key)
                reveal_type(default)
                return default

            def link(node, weight):
                reveal_type(node)
                return node

            def close(handle):
                pass
            ",
        );
        test.with_extra_file(
            "main.pyi",
            "from typing import overload, type_check_only\n\
             class Node: ...\n\
             @overload\ndef get(key: str, default: int) -> int: ...\n\
             @overload\ndef get(key: str, default: str) -> str: ...\n\
             def link(node: Node, weight: float) -> Node: ...\n\
             @type_check_only\nclass _Closer: ...\n\
             def close(handle: _Closer) -> None: ...\n",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inherited_parameter_types: true,
            inferred_return_types: true,
            ..stub_settings()
        }));
        // and the checker reads the body as the hints say
        let file = ProgramFile::new(
            &test.db,
            test.file,
            test.db.program_environment().program(&test.db),
        );
        let revealed: Vec<String> = ty_python_semantic::types::check_types(&test.db, file)
            .iter()
            .filter(|diagnostic| diagnostic.id().as_str() == "revealed-type")
            .map(|diagnostic| diagnostic.concise_message().to_string())
            .collect();
        assert_eq!(
            revealed,
            [
                "Revealed type: `str`",
                "Revealed type: `int | str`",
                "Revealed type: `Node`"
            ]
        );
    }

    #[test]
    fn stub_types_take_the_version_branch_that_runs() {
        let mut test = inlay_hint_test(
            "
            def f(a):
                pass
            ",
        );
        test.with_extra_file(
            "main.pyi",
            "import sys\nif sys.version_info >= (3, 0):\n    def f(a: int) -> None: ...\n\
             else:\n    def f(a: str) -> None: ...\n",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&stub_settings()), @"

        def f(a[: int])[ -> None]:
            pass

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.pyi:3:14
          |
        3 |     def f(a: int) -> None: ...
          |              ^^^
        info: Source
         --> main2.py:2:11
          |
        2 | def f(a[: int])[ -> None]:
          |           ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.pyi:3:22
          |
        3 |     def f(a: int) -> None: ...
          |                      ^^^^
        info: Source
         --> main2.py:2:21
          |
        2 | def f(a[: int])[ -> None]:
          |                     ^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        1 |
          - def f(a):
        2 + def f(a: int) -> None:
        3 |     pass
          |
        ");
    }

    #[test]
    fn stub_bases_go_where_a_class_lists_none() {
        let mut test = inlay_hint_test(
            "
            class Base: ...

            class A:
                pass

            class B():
                pass

            class C(Base):
                pass
            ",
        );
        test.with_extra_file(
            "main.pyi",
            "class Base: ...\nclass A(Base): ...\nclass B(Base, metaclass=type): ...\n\
             class C(object): ...\n",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&stub_settings()), @"

        class Base: ...

        class A[(Base)]:
            pass

        class B([Base, metaclass=type]):
            pass

        class C(Base):
            pass

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.pyi:2:8
          |
        2 | class A(Base): ...
          |        ^^^^^^
        info: Source
         --> main2.py:4:9
          |
        4 | class A[(Base)]:
          |         ^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.pyi:3:8
          |
        3 | class B(Base, metaclass=type): ...
          |        ^^^^^^^^^^^^^^^^^^^^^^
        info: Source
         --> main2.py:7:10
          |
        7 | class B([Base, metaclass=type]):
          |          ^^^^^^^^^^^^^^^^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        3 |
          - class A:
        4 + class A(Base):
        5 |     pass
        6 |
          - class B():
        7 + class B(Base, metaclass=type):
        8 |     pass
          |
        ");
    }

    #[test]
    fn a_slot_the_stub_fills_gets_no_inferred_hint() {
        let mut test = sound_types_inlay_hint_test(
            "
            def f(a):
                return a.name

            def g(b):
                return b.name
            ",
        );
        test.with_extra_file("main.byi", "def f(a: str) -> int: ...\n");

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            stub_types: true,
            inferred_return_types: true,
            inferred_parameter_types: true,
            ..InlayHintSettings::none()
        }), @"

        def f(a[: str])[ -> int]:
            return a.name

        def g(b[: some protocol(let name: Unknown)]):
            return b.name

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.byi:1:10
          |
        1 | def f(a: str) -> int: ...
          |          ^^^
        info: Source
         --> main2.py:2:11
          |
        2 | def f(a[: str])[ -> int]:
          |           ^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.byi:1:18
          |
        1 | def f(a: str) -> int: ...
          |                  ^^^
        info: Source
         --> main2.py:2:21
          |
        2 | def f(a[: str])[ -> int]:
          |                     ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/ty_extensions/_internal.pyi:LL:1
           |
        LL | Unknown: _SpecialForm
           | ^^^^^^^
        info: Source
          --> main2.py:LL:35
           |
        LL | def g(b[: some protocol(let name: Unknown)]):
           |                                   ^^^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.by:1:1
          |
        1 |
          - def f(a):
        2 + def f(a: str) -> int:
        3 |     return a.name
          |
        ");
    }

    #[test]
    fn a_basedpython_stub_is_shown_in_python_to_a_python_module() {
        let mut test = inlay_hint_test(
            "
            def f(a):
                pass
            ",
        );
        test.with_extra_file("main.byi", "def f(a: int?) -> (int) -> str: ...\n");

        assert_snapshot!(test.inlay_hints_with_settings(&stub_settings()), @"

        def f(a[: int | None])[ -> (int, /) -> str]:
            pass

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.byi:1:10
          |
        1 | def f(a: int?) -> (int) -> str: ...
          |          ^^^^
        info: Source
         --> main2.py:2:11
          |
        2 | def f(a[: int | None])[ -> (int, /) -> str]:
          |           ^^^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.byi:1:19
          |
        1 | def f(a: int?) -> (int) -> str: ...
          |                   ^^^^^^^^^^^^
        info: Source
         --> main2.py:2:28
          |
        2 | def f(a[: int | None])[ -> (int, /) -> str]:
          |                            ^^^^^^^^^^^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        1 |
          - def f(a):
        2 + def f(a: int | None):
        3 |     pass
          |
        ");
    }

    #[test]
    fn stub_only_members_go_where_the_stub_puts_them() {
        let mut test = inlay_hint_test(
            "
            import os

            class Parser:
                def parse(self, text):
                    return text.split()

            class Empty:
                pass

            def last(): ...
            ",
        );
        test.with_extra_file(
            "main.pyi",
            "from typing import TypeVar\n\
             _T = TypeVar('_T')\n\
             VERSION: str\n\
             class Parser:\n    \
                 def __init__(self, strict: bool = ...) -> None: ...\n    \
                 def parse(self, text: str) -> list[str]: ...\n    \
                 def reset(self) -> None: ...\n\
             class Empty:\n    size: int\n\
             def _private() -> None: ...\n\
             def last() -> None: ...\n\
             class Tail:\n    \
                 @property\n    \
                 def name(self) -> str: ...\n",
        );

        assert_snapshot!(test.block_hints(&stub_settings()), @r#"
        above 4: (stubOnlyMembers) -> /main.pyi@"VERSION"
        VERSION: str
        above 5: (stubOnlyMembers) -> /main.pyi@"__init__"
            def __init__(self, strict: bool = ...) -> None: ...
        below 6: (stubOnlyMembers) -> /main.pyi@"reset"
            def reset(self) -> None: ...
        below 9: (stubOnlyMembers) -> /main.pyi@"size"
            size: int
        below 11: (stubOnlyMembers) -> /main.pyi@"Tail"
        class Tail:
            @property
            def name(self) -> str: ...
        "#);
        // the edits put each member where its hint is drawn
        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            stub_only_members: true,
            ..InlayHintSettings::none()
        }), @"

        import os

        [VERSION: str]class Parser:
        [    def __init__(self, strict: bool = ...) -> None: ...]    def parse(self, text):
                return text.split()[    def reset(self) -> None: ...]

        class Empty:
            pass[    size: int]

        def last(): ...[class Tail:
            @property
            def name(self) -> str: ...]

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.pyi:3:1
          |
        3 | VERSION: str
          | ^^^^^^^
        info: Source
         --> main2.py:4:2
          |
        4 | [VERSION: str]class Parser:
          |  ^^^^^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.pyi:5:9
          |
        5 |     def __init__(self, strict: bool = ...) -> None: ...
          |         ^^^^^^^^
        info: Source
         --> main2.py:5:2
          |
        5 | [    def __init__(self, strict: bool = ...) -> None: ...]    def parse(self, text):
          |  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.pyi:7:9
          |
        7 |     def reset(self) -> None: ...
          |         ^^^^^
        info: Source
         --> main2.py:6:29
          |
        6 |         return text.split()[    def reset(self) -> None: ...]
          |                             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> main.pyi:9:5
          |
        9 |     size: int
          |     ^^^^
        info: Source
         --> main2.py:9:10
          |
        9 |     pass[    size: int]
          |          ^^^^^^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> main.pyi:12:7
           |
        12 | class Tail:
           |       ^^^^
        info: Source
          --> main2.py:11:17
           |
        11 |   def last(): ...[class Tail:
           |  _________________^
        12 | |     @property
        13 | |     def name(self) -> str: ...]
           | |______________________________^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        3  |
        4  + VERSION: str
        5  | class Parser:
        6  +     def __init__(self, strict: bool = ...) -> None: ...
        7  |     def parse(self, text):
        8  |         return text.split()
        9  +     def reset(self) -> None: ...
        10 |
        11 | class Empty:
        12 |     pass
        13 +     size: int
        14 |
        15 | def last(): ...
        16 + class Tail:
        17 +     @property
        18 +     def name(self) -> str: ...
           |
        ");
    }

    #[test]
    fn a_module_with_no_stub_gets_no_stub_hints() {
        let mut test = inlay_hint_test(
            "
            def f(a):
                pass
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&stub_settings()), @"
        def f(a):
            pass
        ");
    }

    /// a recovered predicate is shown as the place its body narrows. only one about the first
    /// argument, which a `TypeIs` narrows, can be written down unchanged, so only that hint
    /// offers its text
    #[test]
    fn basedpython_inferred_predicate_return_names_its_place() {
        let mut test = basedpython_inlay_hint_test(
            "
            class A:
                a: object

            def member(a: A):
                return a.a is int

            def second(x: object, y: object):
                return y is int

            def first(x: object):
                return x is int

            def not_none(x: int | None):
                return x is not None
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_return_types: true,
            ..InlayHintSettings::none()
        }), @"

        class A:
            a: object

        def member(a: A)[ -> a.a is int]:
            return a.a is int

        def second(x: object, y: object)[ -> y is int]:
            return y is int

        def first(x: object)[ -> x is int]:
            return x is int

        def not_none(x: int | None)[ -> x is (not None)]:
            return x is not None

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:29
           |
        LL | def member(a: A)[ -> a.a is int]:
           |                             ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:43
           |
        LL | def second(x: object, y: object)[ -> y is int]:
           |                                           ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:31
           |
        LL | def first(x: object)[ -> x is int]:
           |                               ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/types.byi:LL:13
           |
        LL | final class NoneType:
           |             ^^^^^^^^
        info: Source
          --> main2.py:LL:43
           |
        LL | def not_none(x: int | None)[ -> x is (not None)]:
           |                                           ^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.by:1:1
           |
        10 |
           - def first(x: object):
        11 + def first(x: object) -> x is int:
        12 |     return x is int
           |
        ");
    }

    /// python has no spelling for a predicate on anything but the first argument, so only that
    /// one is hinted, as the `TypeIs[T]` that narrows it
    #[test]
    fn inferred_predicate_return_types() {
        let mut test = return_type_hint_test(
            "
            class A:
                a: object

            def member(a: A):
                return isinstance(a.a, int)

            def second(x: object, y: object):
                return isinstance(y, int)

            def first(x: object):
                return isinstance(x, int)

            def keyword(*, x: object):
                return isinstance(x, int)

            class C:
                def method(self, x: object):
                    return isinstance(x, int)

                def receiver(self):
                    return isinstance(self, int)

                @staticmethod
                def static(x: object):
                    return isinstance(x, int)

                @classmethod
                def klass(cls, x: object):
                    return isinstance(x, int)
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_return_types: true,
            ..InlayHintSettings::none()
        }), @"

        class A:
            a: object

        def member(a: A):
            return isinstance(a.a, int)

        def second(x: object, y: object):
            return isinstance(y, int)

        def first(x: object)[ -> TypeIs[int]]:
            return isinstance(x, int)

        def keyword(*, x: object):
            return isinstance(x, int)

        class C:
            def method(self, x: object)[ -> TypeIs[int]]:
                return isinstance(x, int)

            def receiver(self):
                return isinstance(self, int)

            @staticmethod
            def static(x: object)[ -> TypeIs[int]]:
                return isinstance(x, int)

            @classmethod
            def klass(cls, x: object)[ -> TypeIs[int]]:
                return isinstance(x, int)

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing_extensions.byi:LL:9
           |
        LL |         TypeIs,
           |         ^^^^^^
        info: Source
          --> main2.py:LL:26
           |
        LL | def first(x: object)[ -> TypeIs[int]]:
           |                          ^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:33
           |
        LL | def first(x: object)[ -> TypeIs[int]]:
           |                                 ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing_extensions.byi:LL:9
           |
        LL |         TypeIs,
           |         ^^^^^^
        info: Source
          --> main2.py:LL:37
           |
        LL |     def method(self, x: object)[ -> TypeIs[int]]:
           |                                     ^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:44
           |
        LL |     def method(self, x: object)[ -> TypeIs[int]]:
           |                                            ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing_extensions.byi:LL:9
           |
        LL |         TypeIs,
           |         ^^^^^^
        info: Source
          --> main2.py:LL:31
           |
        LL |     def static(x: object)[ -> TypeIs[int]]:
           |                               ^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:38
           |
        LL |     def static(x: object)[ -> TypeIs[int]]:
           |                                      ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/typing_extensions.byi:LL:9
           |
        LL |         TypeIs,
           |         ^^^^^^
        info: Source
          --> main2.py:LL:35
           |
        LL |     def klass(cls, x: object)[ -> TypeIs[int]]:
           |                                   ^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:42
           |
        LL |     def klass(cls, x: object)[ -> TypeIs[int]]:
           |                                          ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
           |
        1  + from typing_extensions import TypeIs
        2  +
        3  |
        --------------------------------------------------------------------------------
        12 |
           - def first(x: object):
        13 + def first(x: object) -> TypeIs[int]:
        14 |     return isinstance(x, int)
        --------------------------------------------------------------------------------
        19 | class C:
           -     def method(self, x: object):
        20 +     def method(self, x: object) -> TypeIs[int]:
        21 |         return isinstance(x, int)
        --------------------------------------------------------------------------------
        26 |     @staticmethod
           -     def static(x: object):
        27 +     def static(x: object) -> TypeIs[int]:
        28 |         return isinstance(x, int)
        29 |
        30 |     @classmethod
           -     def klass(cls, x: object):
        31 +     def klass(cls, x: object) -> TypeIs[int]:
        32 |         return isinstance(x, int)
           |
        ");
    }

    /// `override` is written where the hint sits, ahead of any modifier and of `async`, and the
    /// tooltip names the member it overrides
    #[test]
    fn an_inferred_override_writes_itself_and_names_the_overridden_member() {
        let mut test = basedpython_inlay_hint_test(
            r#"
            class Animal:
                def speak(self) -> str:
                    return ""
                async def run(self):
                    pass

            class Dog(Animal):
                def speak(self) -> str:
                    return "woof"
                final async def run(self):
                    pass
            "#,
        );

        assert_snapshot!(test.edits_applied_alone(), @"
        «override » Override
              override def speak(self) -> str:
        «override » Override
              override final async def run(self):
        ");
        assert_snapshot!(test.tooltips(), @"
        «override »: Overrides `Animal.speak`
        «override »: Overrides `Animal.run`
        ");
    }

    /// a variance keyword is written before the name, and only where a parameter can declare
    /// one: not on a pack, nor on a reified parameter, written or inferred
    #[test]
    fn an_inferred_variance_writes_itself_where_it_can() {
        let mut test = basedpython_inlay_hint_test(
            "
            class Source[T]:
                def get(self) -> T: ...

            class Sink[T]:
                def put(self, value: T) -> None: ...

            class Both[T]:
                value: T

            class Packed[*Ts]:
                def get(self) -> tuple[*Ts]: ...

            class Written[reified T]:
                def get(self) -> T: ...

            class Inferred[T]:
                def get(self) -> T: ...
                def kind(self):
                    print(T)
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_variance: true,
            inferred_reification: true,
            ..InlayHintSettings::none()
        }), @"

        class Source[[out ]T]:
            def get(self) -> T: ...

        class Sink[[in ]T]:
            def put(self, value: T) -> None: ...

        class Both[[in out ]T]:
            value: T

        class Packed[[out ]*Ts]:
            def get(self) -> tuple[*Ts]: ...

        class Written[reified [in out ]T]:
            def get(self) -> T: ...

        class Inferred[[reified ][in out ]T]:
            def get(self) -> T: ...
            def kind(self):
                print(T)

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.by:1:1
           |
        1  |
           - class Source[T]:
        2  + class Source[out T]:
        3  |     def get(self) -> T: ...
        4  |
           - class Sink[T]:
        5  + class Sink[in T]:
        6  |     def put(self, value: T) -> None: ...
        7  |
           - class Both[T]:
        8  + class Both[in out T]:
        9  |     value: T
        --------------------------------------------------------------------------------
        16 |
           - class Inferred[T]:
        17 + class Inferred[reified T]:
        18 |     def get(self) -> T: ...
           |
        ");
        assert_snapshot!(test.edits_applied_alone(), @"
        «out » Variance
          class Source[out T]:
        «in » Variance
          class Sink[in T]:
        «in out » Variance
          class Both[in out T]:
        «reified » Reification
          class Inferred[reified T]:
        ");
    }

    /// the inferred exception set is written as a `raises` clause, importing an exception the file
    /// does not bind, and not written at all when it names a class only a function body declares;
    /// the tooltip lists where each one escapes from
    #[test]
    fn an_inferred_raises_writes_itself_with_its_imports() {
        let mut test = basedpython_inlay_hint_test(
            r#"
            from errors import parse

            def load(text: str) -> int:
                if not text:
                    raise ValueError("empty")
                return parse(text)

            def local():
                class Local(Exception): ...
                raise Local()
            "#,
        );
        test.with_extra_file(
            "errors.by",
            "class ParseError(Exception): ...\n\ndef parse(text: str) -> int:\n    raise ParseError()\n",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inferred_raises: true,
            ..InlayHintSettings::none()
        }), @r#"

        from errors import parse

        def load(text: str) -> int[ raises ValueError | ParseError]:
            if not text:
                raise ValueError("empty")
            return parse(text)

        def local()[ raises Local]:
            class Local(Exception): ...
            raise Local()

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class ValueError(Exception):
           |       ^^^^^^^^^^
        info: Source
          --> main2.py:LL:36
           |
        LL | def load(text: str) -> int[ raises ValueError | ParseError]:
           |                                    ^^^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
         --> errors.by:1:7
          |
        1 | class ParseError(Exception): ...
          |       ^^^^^^^^^^
        info: Source
         --> main2.py:4:49
          |
        4 | def load(text: str) -> int[ raises ValueError | ParseError]:
          |                                                 ^^^^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> main.by:10:11
           |
        10 |     class Local(Exception): ...
           |           ^^^^^
        info: Source
         --> main2.py:9:21
          |
        9 | def local()[ raises Local]:
          |                     ^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.by:1:1
          |
        1 |
          - from errors import parse
        2 + from errors import ParseError, parse
        3 |
          - def load(text: str) -> int:
        4 + def load(text: str) -> int raises ValueError | ParseError:
        5 |     if not text:
          |
        "#);
        assert_snapshot!(test.edits_applied_alone(), @"
        « raises ValueError | ParseError» Raises
          from errors import ParseError, parse
          def load(text: str) -> int raises ValueError | ParseError:
        « -> Never» InferredReturnType
          def local() -> Never:
        ");
        assert_snapshot!(test.tooltips(), @r#"
        « raises ValueError | ParseError»: Raised by:
        - line 6, `raise ValueError("empty")`: `ValueError`
        - line 7, `parse(text)`: `ParseError`
        « raises Local»: Raised by:
        - line 11, `raise Local()`: `Local`
        "#);
    }

    /// an inherited annotation is written as the source would write it: spaced around a default,
    /// importing what the base's module names, and on a pack as each element's; the tooltip names
    /// where it comes from
    #[test]
    fn an_inherited_parameter_type_writes_itself() {
        let mut test = basedpython_inlay_hint_test(
            "
            from shapes import Shape

            class Circle(Shape):
                def scale(self, factor, origin=None, *rest):
                    return self

            def area(r: int) -> int: ...
            def area(r: str) -> str: ...
            def area(r):
                return r
            ",
        );
        test.with_extra_file(
            "shapes.by",
            "from decimal import Decimal\n\nclass Shape:\n    def scale(self, factor: Decimal, origin: tuple[int, int] | None = None, *rest: int) -> Shape:\n        return self\n",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inherited_parameter_types: true,
            ..InlayHintSettings::none()
        }), @"

        from shapes import Shape

        class Circle(Shape):
            def scale(self, factor[: Decimal], origin[: (int, int) | None]=None, *rest[: int]):
                return self

        def area(r: int) -> int: ...
        def area(r: str) -> str: ...
        def area(r):
            return r

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/decimal.byi:LL:7
           |
        LL | class Decimal:
           |       ^^^^^^^
        info: Source
          --> main2.py:LL:30
           |
        LL |     def scale(self, factor[: Decimal], origin[: (int, int) | None]=None, *rest[: int]):
           |                              ^^^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class tuple[out Element](Sequence[Element]):
           |       ^^^^^
        info: Source
          --> main2.py:LL:49
           |
        LL |     def scale(self, factor[: Decimal], origin[: (int, int) | None]=None, *rest[: int]):
           |                                                 ^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:50
           |
        LL |     def scale(self, factor[: Decimal], origin[: (int, int) | None]=None, *rest[: int]):
           |                                                  ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:55
           |
        LL |     def scale(self, factor[: Decimal], origin[: (int, int) | None]=None, *rest[: int]):
           |                                                       ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/types.byi:LL:13
           |
        LL | final class NoneType:
           |             ^^^^^^^^
        info: Source
          --> main2.py:LL:62
           |
        LL |     def scale(self, factor[: Decimal], origin[: (int, int) | None]=None, *rest[: int]):
           |                                                              ^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:82
           |
        LL |     def scale(self, factor[: Decimal], origin[: (int, int) | None]=None, *rest[: int]):
           |                                                                                  ^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.by:1:1
          |
        1 |
        2 + from decimal import Decimal
        3 +
        4 | from shapes import Shape
        5 |
        6 | class Circle(Shape):
          -     def scale(self, factor, origin=None, *rest):
        7 +     def scale(self, factor: Decimal, origin: (int, int) | None = None, *rest: int):
        8 |         return self
          |
        ");
        assert_snapshot!(test.edits_applied_alone(), @"
        «override » Override
              override def scale(self, factor, origin=None, *rest):
        «: Decimal» InheritedParameterType
          from decimal import Decimal
              def scale(self, factor: Decimal, origin=None, *rest):
        «: (int, int) | None» InheritedParameterType
              def scale(self, factor, origin: (int, int) | None = None, *rest):
        «: int» InheritedParameterType
              def scale(self, factor, origin=None, *rest: int):
        « -> Shape» InferredReturnType
              def scale(self, factor, origin=None, *rest) -> Shape:
        ");
        assert_snapshot!(test.tooltips(), @"
        «override »: Overrides `Shape.scale`
        «: Decimal»: Inherited from `Shape.scale`
        «: (int, int) | None»: Inherited from `Shape.scale`
        «: int»: Inherited from `Shape.scale`
        ");
    }

    /// basedpython's lambda takes annotations in parentheses, which the edit adds where the
    /// lambda has none; a python lambda takes none at all
    #[test]
    fn a_lambda_parameter_type_writes_itself_in_basedpython() {
        let mut test = basedpython_inlay_hint_test(
            "
            def apply(f: (int, str) -> str) -> None: ...

            apply(lambda n, s: s * n)
            apply(lambda (n, s=''): s * n)
            ",
        );

        assert_snapshot!(test.edits_applied_alone(), @"
        «f=» CallArgumentName
          apply(f=lambda n, s: s * n)
        «: int» LambdaParameterType
          apply(lambda (n: int, s): s * n)
        «: str» LambdaParameterType
          apply(lambda (n, s: str): s * n)
        «f=» CallArgumentName
          apply(f=lambda (n, s=''): s * n)
        «: int» LambdaParameterType
          apply(lambda (n: int, s=''): s * n)
        «: str» LambdaParameterType
          apply(lambda (n, s: str = ''): s * n)
        ");

        let mut test = inlay_hint_test(
            "
            from typing import Callable
            def apply(f: Callable[[int], str]) -> None: ...

            apply(lambda n: str(n))
            ",
        );
        assert_snapshot!(test.edits_applied_alone(), @"
        «f=» CallArgumentName
          apply(f=lambda n: str(n))
        «object=» CallArgumentName
          apply(lambda n: str(object=n))
        ");
    }

    /// a `some` bound, a property's type and a base's type arguments are written where they are
    /// shown
    #[test]
    fn annotations_basedpython_infers_write_themselves() {
        let mut test = basedpython_inlay_hint_test(
            "
            def greet(person, times=1):
                name: str = person.name
                count: int = times + 1
                return name * count

            class Temperature:
                celsius: float
                let fahrenheit
                    get() = self.celsius * 9 / 5 + 32

            class Box[T]:
                item: T

            class IntBox(Box):
                item = 0
            ",
        );

        assert_snapshot!(test.edits_applied_alone(), @"
        «: some protocol(let name: str)» InferredParameterType
          def greet(person: some protocol(let name: str), times=1):
        «: some int» InferredParameterType
          def greet(person, times: some int = 1):
        « -> str» InferredReturnType
          def greet(person, times=1) -> str:
        «: float» PropertyType
              let fahrenheit: float
        «in out » Variance
          class Box[in out T]:
        «[int]» InferredBaseTypeArguments
          class IntBox(Box[int]):
        ");
    }

    /// a call is specialized where its type arguments are shown: any generic callee in a `.by`
    /// file, but only a class in a `.py` file, where a function has no `__getitem__`
    #[test]
    fn call_type_arguments_write_themselves_where_the_callee_takes_them() {
        let mut test = basedpython_inlay_hint_test(
            "
            def first[T](items: list[T]) -> T:
                return items[0]

            class Pair[A, B]:
                def __init__(self, a: A, b: B) -> None: ...

            top = first([3.0, 1.0])
            pair = Pair(1.5, 'x')
            ",
        );
        assert_snapshot!(test.edits_applied_alone(), @r#"
        «out » Variance
          class Pair[out A, B]:
        «out » Variance
          class Pair[A, out B]:
        «: float» VariableType
          top: float = first([3.0, 1.0])
        «[float]» CallTypeArguments
          top = first[float]([3.0, 1.0])
        «items=» CallArgumentName
          top = first(items=[3.0, 1.0])
        «: Pair[A=1.5, B="x"]» VariableType
          pair: Pair[A=1.5, B="x"] = Pair(1.5, 'x')
        «[A=1.5, B="x"]» CallTypeArguments
          pair = Pair[A=1.5, B="x"](1.5, 'x')
        «a=» CallArgumentName
          pair = Pair(a=1.5, b='x')
        «b=» CallArgumentName
          pair = Pair(1.5, b='x')
        "#);

        let mut test = inlay_hint_test(
            "
            def first[T](items: list[T]) -> T:
                return items[0]

            class Box[T]:
                def __init__(self, item: T) -> None: ...

            top = first([3.0, 1.0])
            box = Box(1.5)
            ",
        );
        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            call_type_arguments: true,
            ..InlayHintSettings::none()
        }), @"

        def first[T](items: list[T]) -> T:
            return items[0]

        class Box[T]:
            def __init__(self, item: T) -> None: ...

        top = first[[int | float]]([3.0, 1.0])
        box = Box[[float]](1.5)

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ^^^
        info: Source
          --> main2.py:LL:14
           |
        LL | top = first[[int | float]]([3.0, 1.0])
           |              ^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class float:
           |       ^^^^^
        info: Source
          --> main2.py:LL:20
           |
        LL | top = first[[int | float]]([3.0, 1.0])
           |                    ^^^^^

        info[inlay-hint-location]: Inlay Hint Target
          --> stdlib/builtins.byi:LL:7
           |
        LL | class float:
           |       ^^^^^
        info: Source
          --> main2.py:LL:12
           |
        LL | box = Box[[float]](1.5)
           |            ^^^^^

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.py:1:1
          |
        8 | top = first([3.0, 1.0])
          - box = Box(1.5)
        9 + box = Box[float](1.5)
          |
        ");
    }

    /// an inherited default is a literal, which reads the same anywhere, so it is written where
    /// it is shown; an enum member, which would be written through a class this file may not
    /// bind, is not shown at all. the tooltip names the method it comes from
    #[test]
    fn an_inherited_default_writes_itself() {
        let mut test = basedpython_inlay_hint_test(
            r#"
            from colors import Color, Painter

            class Greeter:
                def greet(self, name: str = "world", times=2) -> str:
                    return name

            class LoudGreeter(Greeter):
                def greet(self, name: str, times) -> str:
                    return name.upper()

            class Brush(Painter):
                def paint(self, color) -> None: ...
            "#,
        );
        test.with_extra_file(
            "colors.by",
            "from enum import Enum\n\nclass Color(Enum):\n    RED = 1\n\nclass Painter:\n    def paint(self, color: Color = Color.RED) -> None: ...\n",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            inherited_parameter_defaults: true,
            ..InlayHintSettings::none()
        }), @r#"

        from colors import Color, Painter

        class Greeter:
            def greet(self, name: str = "world", times=2) -> str:
                return name

        class LoudGreeter(Greeter):
            def greet(self, name: str[ = "world"], times[=2]) -> str:
                return name.upper()

        class Brush(Painter):
            def paint(self, color) -> None: ...

        ---------------------------------------------
        info[inlay-hint-edit]: Inlay hint edits
        --> main.by:1:1
           |
        8  | class LoudGreeter(Greeter):
           -     def greet(self, name: str, times) -> str:
        9  +     def greet(self, name: str = "world", times=2) -> str:
        10 |         return name.upper()
           |
        "#);
        assert_snapshot!(test.edits_applied_alone(), @r#"
        «: some int» InferredParameterType
              def greet(self, name: str = "world", times: some int = 2) -> str:
        «override » Override
              override def greet(self, name: str, times) -> str:
        « = "world"» InheritedDefault
              def greet(self, name: str = "world", times) -> str:
        «=2» InheritedDefault
              def greet(self, name: str, times=2) -> str:
        «override » Override
              override def paint(self, color) -> None: ...
        «: Color» InheritedParameterType
              def paint(self, color: Color) -> None: ...
        "#);
        assert_snapshot!(test.tooltips(), @r#"
        «override »: Overrides `Greeter.greet`
        « = "world"»: Inherited from `Greeter.greet`
        «=2»: Inherited from `Greeter.greet`
        «override »: Overrides `Painter.paint`
        «: Color»: Inherited from `Painter.paint`
        "#);
    }

    /// every hinted positional argument writes its name, and every positional argument's after
    /// it, since a positional argument cannot follow a named one. one followed by an unpacked
    /// argument, or by one passed to a parameter that takes no name, writes nothing; the tooltip
    /// is the parameter's declaration
    #[test]
    fn call_argument_names_write_themselves_and_every_one_after() {
        let mut test = inlay_hint_test(
            "
            def connect(host: str, port: int = 80, timeout: float = 1.0): ...
            def spread(a: int, b: int, c: int): ...
            def only(a: int, /, b: int, *args: int): ...

            port = 8080
            connect('localhost', port, 2.5)
            t: tuple[int] = (2,)
            spread(1, *t, 3)
            only(1, 2, 3)
            ",
        );

        assert_snapshot!(test.edits_applied_alone(), @"
        «host=» CallArgumentName
          connect(host='localhost', port=port, timeout=2.5)
        «timeout=» CallArgumentName
          connect('localhost', port, timeout=2.5)
        «c=» CallArgumentName
          spread(1, *t, c=3)
        ");
        assert_snapshot!(test.tooltips(), @"
        «host=»: ```python
        host: str
        ```
        «timeout=»: ```python
        timeout: float = 1.0
        ```
        «a=»: ```python
        a: int
        ```
        «b=»: ```python
        b: int
        ```
        «c=»: ```python
        c: int
        ```
        «b=»: ```python
        b: int
        ```
        ");
    }

    /// the block of a trailing lambda is passed after the arguments the call writes, so none of
    /// them is named
    #[test]
    fn a_trailing_lambda_callee_names_no_argument() {
        let mut test = basedpython_inlay_hint_test(
            "
            def repeat(times: int, action: (int) -> None): ...

            repeat(3):
                print(it)
            ",
        );

        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            call_argument_names: true,
            ..InlayHintSettings::none()
        }), @"

        def repeat(times: int, action: (int) -> None): ...

        repeat([times=]3):
            print(it)

        ---------------------------------------------
        info[inlay-hint-location]: Inlay Hint Target
         --> main.by:2:12
          |
        2 | def repeat(times: int, action: (int) -> None): ...
          |            ^^^^^
        info: Source
         --> main2.py:4:9
          |
        4 | repeat([times=]3):
          |         ^^^^^
        ");
    }

    /// basedpython's keyword subscript names a type argument as a call names an argument, so a
    /// type argument name writes itself and every one after it; python's subscript has no keyword
    #[test]
    fn type_argument_names_write_themselves_in_basedpython() {
        let mut test = basedpython_inlay_hint_test(
            "
            scores: dict[str, int] = {}
            ",
        );
        assert_snapshot!(test.edits_applied_alone(), @"
        «Key=» TypeArgumentName
          scores: dict[Key=str, Value=int] = {}
        «Value=» TypeArgumentName
          scores: dict[str, Value=int] = {}
        ");

        let mut test = inlay_hint_test(
            "
            scores: dict[str, int] = {}
            ",
        );
        assert_snapshot!(test.inlay_hints_with_settings(&InlayHintSettings {
            type_argument_names: true,
            ..InlayHintSettings::none()
        }), @"

        scores: dict[[Key=]str, [Value=]int] = {}
        ");
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
