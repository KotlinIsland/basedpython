//! hints in a runtime module drawn from the stub that describes it
//!
//! a stub says what its runtime module declares, often more than the module spells: the
//! annotations a `.py` leaves out, and members the module binds in ways a reader can't see (a C
//! accelerator, a `globals()` update, a `__getattr__`). two kinds of hint show what the stub says
//! where the runtime module would say it:
//!
//! - `stubTypes`: a parameter annotation, a return annotation or the bases the stub declares for a
//!   `def` or a `class` that does not spell them, where they would be written. the stub's
//!   declaration is what the slot gets: a slot the stub covers gets no inferred hint beside it.
//!   a function the stub declares several times (overloads, or a declaration in each branch of a
//!   version check that runs) gets a hint in a slot only where every declaration says the same
//!   there: an overloaded `def f(x: int) -> int` / `def f(x: str) -> str` hints neither `x` nor the
//!   return, since the one runtime `def` implements both and no single annotation is the stub's.
//! - `stubOnlyMembers`: a member the stub declares and the runtime module (or class) does not,
//!   drawn as its own lines, the stub's source as the stub writes it, where the stub's order puts
//!   it among the runtime's members.

use ruff_db::files::File;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::{SourceText, source_text};
use ruff_python_ast::comparable::ComparableExpr;
use ruff_python_ast::helpers::is_dunder;
use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{self as ast, AnyParameterRef, Expr, Stmt};
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::FxHashMap;
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::place::ScopedPlaceId;
use ty_python_core::{ProgramFile, global_scope, place_table};
use ty_python_semantic::stub_mapping::{
    PairedMember, is_definition_reachable, runtime_module_stub, stub_pairing,
};
use ty_python_semantic::{HasType, SemanticModel};

use super::{
    BlockPlacement, InlayHint, InlayHintImportContext, InlayHintKind, InlayHintLabel,
    InlayHintLabelPart, InlayHintTextEdit, InlayHintVisitor, spell_type, type_display_details,
};
use crate::{Db, NavigationTarget};

/// the stub of the runtime module being hinted, and the declarations it pairs with the module's
pub(super) struct StubContext<'db> {
    stub: ProgramFile<'db>,
    stub_model: SemanticModel<'db>,
    stub_module: ParsedModuleRef,
    stub_source: SourceText,
    /// whether the stub and the runtime module are written in the same language, so that an
    /// annotation the stub spells can be written into the runtime module as it is
    same_language: bool,
    /// each runtime `def` and `class` the stub declares too, with the stub's declarations of it
    /// in the code that runs for the configured version and platform
    counterparts: FxHashMap<Definition<'db>, Vec<Definition<'db>>>,
}

impl<'db> StubContext<'db> {
    /// `None` for a module no stub describes
    pub(super) fn new(db: &'db dyn Db, runtime: ProgramFile<'db>) -> Option<Self> {
        let stub = runtime_module_stub(db, runtime)?;
        let mut counterparts = FxHashMap::default();
        collect_counterparts(db, stub_pairing(db, stub, runtime), &mut counterparts);
        Some(Self {
            stub,
            stub_model: SemanticModel::new(db, stub),
            stub_module: parsed_module(db, stub.python_file(db)).load(db),
            stub_source: source_text(db, stub.file(db)),
            same_language: stub.file(db).source_type(db).is_basedpython()
                == runtime.file(db).source_type(db).is_basedpython(),
            counterparts,
        })
    }

    fn stub_file(&self, db: &dyn Db) -> File {
        self.stub.file(db)
    }

    /// a label part showing `range` of the stub as the stub writes it, leading there
    fn stub_part(
        &self,
        db: &dyn Db,
        text: impl Into<String>,
        range: TextRange,
    ) -> InlayHintLabelPart {
        self.stub_declaration_part(db, text, range, range)
    }

    /// a label part showing a declaration of the stub, leading to the name it declares, `focus`,
    /// within the whole of it, `full`
    fn stub_declaration_part(
        &self,
        db: &dyn Db,
        text: impl Into<String>,
        focus: TextRange,
        full: TextRange,
    ) -> InlayHintLabelPart {
        InlayHintLabelPart::new(text).with_target(Some(NavigationTarget::create(
            db,
            self.stub_file(db),
            focus,
            full,
        )))
    }
}

fn collect_counterparts<'db>(
    db: &'db dyn Db,
    members: &[PairedMember<'db>],
    counterparts: &mut FxHashMap<Definition<'db>, Vec<Definition<'db>>>,
) {
    for member in members {
        if member.is_paired() {
            let stub: Vec<_> = member
                .stub
                .iter()
                .copied()
                .filter(|definition| is_definition_reachable(db, *definition))
                .collect();
            for runtime in member.runtime.iter().copied() {
                let wanted: fn(&DefinitionKind<'_>) -> bool = match runtime.kind(db) {
                    DefinitionKind::Function(_) => {
                        |kind| matches!(kind, DefinitionKind::Function(_))
                    }
                    DefinitionKind::Class(_) => |kind| matches!(kind, DefinitionKind::Class(_)),
                    _ => continue,
                };
                let matching: Vec<_> = stub
                    .iter()
                    .copied()
                    .filter(|definition| wanted(definition.kind(db)))
                    .collect();
                if !matching.is_empty() {
                    counterparts.insert(runtime, matching);
                }
            }
        }
        collect_counterparts(db, &member.members, counterparts);
    }
}

/// what every one of a stub's declarations says in one slot: `Some` only where each says the same
fn agreed<'a, T: ?Sized>(
    declarations: impl IntoIterator<Item = Option<&'a T>>,
    same: impl Fn(&T, &T) -> bool,
) -> Option<&'a T> {
    let mut agreed: Option<&T> = None;
    for declaration in declarations {
        let declaration = declaration?;
        match agreed {
            Some(first) if !same(first, declaration) => return None,
            Some(_) => {}
            None => agreed = Some(declaration),
        }
    }
    agreed
}

fn same_expression(left: &Expr, right: &Expr) -> bool {
    ComparableExpr::from(left) == ComparableExpr::from(right)
}

/// whether two class headers list the same bases and keywords
fn same_arguments(left: &ast::Arguments, right: &ast::Arguments) -> bool {
    left.args.len() == right.args.len()
        && left.keywords.len() == right.keywords.len()
        && left
            .args
            .iter()
            .zip(&right.args)
            .all(|(left, right)| same_expression(left, right))
        && left
            .keywords
            .iter()
            .zip(&right.keywords)
            .all(|(left, right)| {
                left.arg.as_ref().map(|arg| &arg.id) == right.arg.as_ref().map(|arg| &arg.id)
                    && same_expression(&left.value, &right.value)
            })
}

impl<'db> InlayHintVisitor<'_, 'db> {
    /// the stub's declarations of the runtime `def` or `class` declared by `node`
    fn stub_declarations(&self, definition: Option<Definition<'db>>) -> Option<&[Definition<'db>]> {
        let stub = self.stub.as_ref()?;
        stub.counterparts.get(&definition?).map(Vec::as_slice)
    }

    /// `stubTypes`: the parameter and return annotations the stub declares for a `def` that
    /// leaves them out
    pub(super) fn add_stub_types(&mut self, function: &ast::StmtFunctionDef) {
        if !self.settings.enabled(InlayHintKind::StubTypes) {
            return;
        }
        let definition = self.model_definition(function.into());
        let Some(declarations) = self.stub_declarations(definition) else {
            return;
        };
        let Some(stub) = self.stub.as_ref() else {
            return;
        };
        let stub_module = &stub.stub_module;
        let stub_functions: Vec<&ast::StmtFunctionDef> = declarations
            .iter()
            .filter_map(|definition| match definition.kind(self.db) {
                DefinitionKind::Function(function) => Some(function.node(stub_module)),
                _ => None,
            })
            .collect();
        if stub_functions.is_empty() {
            return;
        }

        let mut hints = Vec::new();
        for parameter in &function.parameters {
            let runtime_parameter = parameter.as_parameter();
            if runtime_parameter.annotation.is_some() || runtime_parameter.range().is_empty() {
                continue;
            }
            let name = runtime_parameter.name.id.as_str();
            let stub_parameters: Vec<Option<&ast::Parameter>> = stub_functions
                .iter()
                .map(|stub_function| {
                    stub_function
                        .parameters
                        .iter()
                        .find(|stub_parameter| stub_parameter.name().as_str() == name)
                        .map(AnyParameterRef::as_parameter)
                })
                .collect();
            let Some(annotation) = agreed(
                stub_parameters
                    .iter()
                    .map(|stub_parameter| stub_parameter.and_then(|p| p.annotation.as_deref())),
                same_expression,
            ) else {
                continue;
            };
            let Some(Some(stub_parameter)) = stub_parameters.first().copied() else {
                continue;
            };
            let declared = stub_parameter.inferred_type(&stub.stub_model);
            hints.push(self.stub_annotation_hint(
                runtime_parameter.name.range().end(),
                parameter,
                annotation,
                declared,
                function,
            ));
        }

        if function.returns.is_none()
            && !function.is_asserts_return
            && let Some(returns) = agreed(
                stub_functions
                    .iter()
                    .map(|stub_function| stub_function.returns.as_deref()),
                same_expression,
            )
        {
            let declared = returns.inferred_type(&stub.stub_model);
            hints.push(self.stub_return_hint(function, returns, declared));
        }

        for hint in hints.into_iter().flatten() {
            let position = hint.position;
            if self.push(hint) {
                self.stub_covered.insert(position);
            }
        }
    }

    /// `stubTypes`: a parameter's annotation, after its name
    fn stub_annotation_hint(
        &self,
        position: TextSize,
        parameter: AnyParameterRef<'_>,
        annotation: &Expr,
        declared: Option<ty_python_semantic::types::Type<'db>>,
        function: &ast::StmtFunctionDef,
    ) -> Option<InlayHint> {
        if !self.range.contains_inclusive(position) {
            return None;
        }
        let stub = self.stub.as_ref()?;
        let text = &stub.stub_source[annotation.range()];
        let annotation_edit = self.annotation_text(annotation, declared, function);

        // `a=1` takes the annotation as `a: int = 1`, the spacing an annotated default is written
        // with
        let text_edits = annotation_edit
            .map(|(written, imports)| {
                let edit = match parameter {
                    AnyParameterRef::NonVariadic(parameter)
                        if let Some(default) = parameter.default.as_deref() =>
                    {
                        InlayHintTextEdit {
                            range: TextRange::new(position, default.start()),
                            new_text: format!(": {written} = "),
                        }
                    }
                    _ => InlayHintTextEdit {
                        range: TextRange::empty(position),
                        new_text: format!(": {written}"),
                    },
                };
                std::iter::once(edit).chain(imports).collect()
            })
            .unwrap_or_default();

        Some(InlayHint {
            position,
            kind: InlayHintKind::StubTypes,
            label: InlayHintLabel {
                parts: vec![
                    ": ".into(),
                    stub.stub_part(self.db, self.label_text(text, declared), annotation.range()),
                ],
            },
            padding_left: false,
            padding_right: false,
            text_edits,
            placement: None,
        })
    }

    /// `stubTypes`: a `def`'s return annotation, after its parameters
    fn stub_return_hint(
        &self,
        function: &ast::StmtFunctionDef,
        returns: &Expr,
        declared: Option<ty_python_semantic::types::Type<'db>>,
    ) -> Option<InlayHint> {
        let position = function.parameters.end();
        if !self.range.contains_inclusive(position) {
            return None;
        }
        let stub = self.stub.as_ref()?;
        let text = &stub.stub_source[returns.range()];
        let text_edits = self
            .annotation_text(returns, declared, function)
            .map(|(written, imports)| {
                std::iter::once(InlayHintTextEdit {
                    range: TextRange::empty(position),
                    new_text: format!(" -> {written}"),
                })
                .chain(imports)
                .collect()
            })
            .unwrap_or_default();
        Some(InlayHint {
            position,
            kind: InlayHintKind::StubTypes,
            label: InlayHintLabel {
                parts: vec![
                    "-> ".into(),
                    stub.stub_part(self.db, self.label_text(text, declared), returns.range()),
                ],
            },
            padding_left: true,
            padding_right: false,
            text_edits,
            placement: None,
        })
    }

    /// what a hint shows of an annotation the stub writes as `text`: the stub's own words, unless
    /// the stub is in another language, which shows the type as this file would write it
    fn label_text(
        &self,
        text: &str,
        declared: Option<ty_python_semantic::types::Type<'db>>,
    ) -> String {
        let same_language = self.stub.as_ref().is_some_and(|stub| stub.same_language);
        match declared {
            Some(declared) if !same_language => declared
                .display(self.db, &self.model.program_environment())
                .to_string(),
            _ => text.to_string(),
        }
    }

    /// the annotation to write into this file for one the stub writes as `annotation`, with the
    /// imports it needs: the stub's words where every name in them means something here already,
    /// otherwise the declared type as this file would spell it, importing what it names. `None`
    /// where neither can be written
    fn annotation_text(
        &self,
        annotation: &Expr,
        declared: Option<ty_python_semantic::types::Type<'db>>,
        function: &ast::StmtFunctionDef,
    ) -> Option<(String, Vec<InlayHintTextEdit>)> {
        let stub = self.stub.as_ref()?;
        if stub.same_language && self.names_are_bound(annotation) {
            return Some((stub.stub_source[annotation.range()].to_string(), Vec::new()));
        }
        let declared = declared?;
        let file = self.model.program_file();
        let context = InlayHintImportContext {
            db: self.db,
            file,
            importer: &self.importer,
        };
        let type_params_in_scope = self.model.type_params_in_signature(function);
        let details = type_display_details(self.db, file, declared, false, &type_params_in_scope);
        let spelled = spell_type(
            context,
            &mut FxHashMap::default(),
            function.into(),
            function.start(),
            &details,
            &type_params_in_scope,
        );
        spelled
            .editable
            .then_some((spelled.edit_label, spelled.import_edits))
    }

    /// whether every name `expression` reads is bound at the top of this module or is a builtin,
    /// so that writing it here means what it means in the stub
    fn names_are_bound(&self, expression: &Expr) -> bool {
        struct Names<'a>(Vec<&'a str>);
        impl<'a> Visitor<'a> for Names<'a> {
            fn visit_expr(&mut self, expr: &'a Expr) {
                if let Expr::Name(name) = expr {
                    self.0.push(name.id.as_str());
                }
                visitor::walk_expr(self, expr);
            }
        }
        let mut names = Names(Vec::new());
        names.visit_expr(expression);

        let file = self.model.program_file();
        let globals = place_table(self.db, global_scope(self.db, file));
        let minor = file.python_version(self.db).minor;
        names.0.iter().all(|name| {
            globals
                .symbol_by_name(name)
                .is_some_and(ty_python_core::symbol::Symbol::is_bound)
                || ruff_python_stdlib::builtins::is_python_builtin(name, minor, false)
        })
    }

    /// `stubTypes`: the bases the stub declares for a class that lists none
    pub(super) fn add_stub_bases(&mut self, class: &ast::StmtClassDef) {
        if !self.settings.enabled(InlayHintKind::StubTypes)
            || class
                .arguments
                .as_ref()
                .is_some_and(|arguments| !arguments.is_empty())
        {
            return;
        }
        let definition = self.model_definition(class.into());
        let Some(declarations) = self.stub_declarations(definition) else {
            return;
        };
        let Some(stub) = self.stub.as_ref() else {
            return;
        };
        let stub_module = &stub.stub_module;
        let Some(arguments) = agreed(
            declarations
                .iter()
                .map(|definition| match definition.kind(self.db) {
                    DefinitionKind::Class(class) => class
                        .node(stub_module)
                        .arguments
                        .as_deref()
                        .filter(|arguments| !arguments.is_empty()),
                    _ => None,
                }),
            same_arguments,
        ) else {
            return;
        };

        let inner = TextRange::new(
            arguments.start() + TextSize::new(1),
            arguments.end() - TextSize::new(1),
        );
        let inner_text = stub.stub_source[inner].trim();
        let (position, label, written) = match class.arguments.as_deref() {
            // `class A():` takes the bases inside the parentheses it has
            Some(empty) => (
                empty.start() + TextSize::new(1),
                inner_text.to_string(),
                inner_text.to_string(),
            ),
            None => (
                class
                    .type_params
                    .as_deref()
                    .map_or(class.name.end(), Ranged::end),
                format!("({inner_text})"),
                format!("({inner_text})"),
            ),
        };
        if !self.range.contains_inclusive(position) {
            return;
        }
        let editable = stub.same_language
            && arguments
                .args
                .iter()
                .chain(arguments.keywords.iter().map(|keyword| &keyword.value))
                .all(|argument| self.names_are_bound(argument));
        let hint = InlayHint {
            position,
            kind: InlayHintKind::StubTypes,
            label: InlayHintLabel {
                parts: vec![stub.stub_part(self.db, label, arguments.range())],
            },
            padding_left: false,
            padding_right: false,
            text_edits: if editable {
                vec![InlayHintTextEdit {
                    range: TextRange::empty(position),
                    new_text: written,
                }]
            } else {
                Vec::new()
            },
            placement: None,
        };
        self.push(hint);
    }

    /// `stubOnlyMembers`: every member the stub declares that the runtime module does not,
    /// module-level and in the classes both declare
    pub(super) fn add_stub_only_members(&mut self, module: &'_ ParsedModuleRef) {
        if !self.settings.enabled(InlayHintKind::StubOnlyMembers) {
            return;
        }
        let Some(stub) = self.stub.as_ref() else {
            return;
        };
        let runtime = self.model.program_file();
        let members = stub_pairing(self.db, stub.stub, runtime);
        let mut hints = Vec::new();
        let container = Container {
            body: module.suite(),
            end: TextSize::of(self.source.trim_end()),
            indentation: String::new(),
        };
        self.stub_only_hints(members, &container, module, &mut hints);
        for hint in hints {
            if self.range.contains_inclusive(hint.position) {
                self.push(hint);
            }
        }
    }

    fn stub_only_hints(
        &self,
        members: &[PairedMember<'db>],
        container: &Container<'_>,
        module: &ParsedModuleRef,
        hints: &mut Vec<InlayHint>,
    ) {
        let Some(stub) = self.stub.as_ref() else {
            return;
        };
        // where each member the runtime declares sits among the container's statements
        let statements: Vec<Option<&Stmt>> = members
            .iter()
            .map(|member| {
                member
                    .runtime
                    .iter()
                    .find(|definition| {
                        matches!(definition.place(self.db), ScopedPlaceId::Symbol(_))
                    })
                    .and_then(|definition| {
                        let range = definition.full_range(self.db, module).range();
                        container
                            .body
                            .iter()
                            .find(|statement| statement.range().contains_range(range))
                    })
            })
            .collect();

        for (index, member) in members.iter().enumerate() {
            if member.is_paired() {
                if let Some(class) =
                    member
                        .runtime
                        .iter()
                        .find_map(|definition| match definition.kind(self.db) {
                            DefinitionKind::Class(class) => Some(class.node(module)),
                            _ => None,
                        })
                {
                    let nested = Container {
                        body: &class.body,
                        end: class.end(),
                        indentation: class
                            .body
                            .first()
                            .map(|first| indentation_at(self.source, first.start()).to_string())
                            .unwrap_or_else(|| format!("{}    ", container.indentation)),
                    };
                    self.stub_only_hints(&member.members, &nested, module, hints);
                }
                continue;
            }
            if !shows_as_stub_only(self.db, member, &stub.stub_module) {
                continue;
            }
            let (position, placement) =
                if let Some(next) = statements[index + 1..].iter().flatten().next() {
                    (self.source.line_start(next.start()), BlockPlacement::Above)
                } else if let Some(previous) = statements[..index].iter().flatten().next_back() {
                    (previous.end(), BlockPlacement::Below)
                } else {
                    (container.end, BlockPlacement::Below)
                };
            for definition in member.stub.iter().copied() {
                if !is_definition_reachable(self.db, definition) {
                    continue;
                }
                let range = declaration_range(self.db, definition, &stub.stub_module);
                let text = reindented(
                    &stub.stub_source,
                    range,
                    &container.indentation,
                    declaration_head(self.db, definition, &stub.stub_module, &stub.stub_source)
                        .as_deref(),
                );
                let text_edits = if stub.same_language {
                    let new_text = match placement {
                        BlockPlacement::Above => format!("{text}\n"),
                        BlockPlacement::Below => format!("\n{text}"),
                    };
                    vec![InlayHintTextEdit {
                        range: TextRange::empty(position),
                        new_text,
                    }]
                } else {
                    Vec::new()
                };
                hints.push(InlayHint {
                    position,
                    kind: InlayHintKind::StubOnlyMembers,
                    label: InlayHintLabel {
                        parts: vec![stub.stub_declaration_part(
                            self.db,
                            text,
                            crate::stub_counterparts::name_range(
                                self.db,
                                &stub.stub_module,
                                definition,
                            ),
                            range,
                        )],
                    },
                    padding_left: false,
                    padding_right: false,
                    text_edits,
                    placement: Some(placement),
                });
            }
        }
    }

    fn model_definition(&self, node: ruff_python_ast::AnyNodeRef<'_>) -> Option<Definition<'db>> {
        let index = ty_python_core::semantic_index(self.db, self.model.program_file());
        match node {
            ruff_python_ast::AnyNodeRef::StmtFunctionDef(function) => {
                index.try_definition(function)
            }
            ruff_python_ast::AnyNodeRef::StmtClassDef(class) => index.try_definition(class),
            _ => None,
        }
    }
}

/// the statements a stub-only member is placed among: a module's, or a class's
struct Container<'a> {
    body: &'a [Stmt],
    /// where the last of it ends, below which a member goes when none of the stub's members around
    /// it is in the runtime module
    end: TextSize,
    /// what each line of a member drawn among the statements starts with
    indentation: String,
}

/// whether a member the runtime module lacks is worth drawing: not one only a type checker reads
///
/// a stub declares a few things for itself that its module has no reason to: a private helper
/// (`_T = TypeVar("_T")`, `class _SupportsRead(Protocol)`), anything `@type_check_only`, a type
/// variable or a type alias. their absence from the runtime module is what a stub is for
fn shows_as_stub_only(
    db: &dyn Db,
    member: &PairedMember<'_>,
    stub_module: &ParsedModuleRef,
) -> bool {
    let name = member.name.as_str();
    if name.starts_with('_') && !is_dunder(name) {
        return false;
    }
    member
        .stub
        .iter()
        .any(|definition| match definition.kind(db) {
            DefinitionKind::Function(function) => {
                !is_type_check_only(&function.node(stub_module).decorator_list)
            }
            DefinitionKind::Class(class) => {
                !is_type_check_only(&class.node(stub_module).decorator_list)
            }
            DefinitionKind::TypeAlias(_) => false,
            DefinitionKind::AnnotatedAssignment(assignment) => {
                !is_named(assignment.annotation(stub_module), "TypeAlias")
            }
            DefinitionKind::Assignment(assignment) => !matches!(
                assignment.value(stub_module),
                Expr::Call(call) if ["TypeVar", "ParamSpec", "TypeVarTuple"]
                    .iter()
                    .any(|name| is_named(&call.func, name))
            ),
            _ => true,
        })
}

fn is_type_check_only(decorators: &[ast::Decorator]) -> bool {
    decorators
        .iter()
        .any(|decorator| is_named(&decorator.expression, "type_check_only"))
}

/// whether `expression` is `name` or an attribute `name` of something, as `typing.TypeAlias`
fn is_named(expression: &Expr, name: &str) -> bool {
    match expression {
        Expr::Name(expression) => expression.id.as_str() == name,
        Expr::Attribute(attribute) => attribute.attr.as_str() == name,
        _ => false,
    }
}

/// the stub source a stub-only declaration is drawn from: a `def` or a `class` with its
/// decorators, a variable's whole declaration, an import's one name
fn declaration_range(
    db: &dyn Db,
    definition: Definition<'_>,
    module: &ParsedModuleRef,
) -> TextRange {
    definition.full_range(db, module).range()
}

/// what an import's one name is drawn after, since the import statement it is part of names
/// others too: `from m import ` for `from m import x as x`
fn declaration_head(
    db: &dyn Db,
    definition: Definition<'_>,
    module: &ParsedModuleRef,
    source: &str,
) -> Option<String> {
    match definition.kind(db) {
        DefinitionKind::ImportFrom(import) => {
            let statement = import.import(module);
            let mut head = String::from("from ");
            for _ in 0..statement.level {
                head.push('.');
            }
            if let Some(module) = &statement.module {
                head.push_str(&source[module.range()]);
            }
            head.push_str(" import ");
            Some(head)
        }
        DefinitionKind::Import(_) => Some("import ".to_string()),
        _ => None,
    }
}

/// the whitespace a line starts with, up to `offset`, when only whitespace comes before `offset`
fn indentation_at(source: &str, offset: TextSize) -> &str {
    let line_start = source.line_start(offset);
    let before = &source[TextRange::new(line_start, offset)];
    if before.chars().all(|c| c == ' ' || c == '\t') {
        before
    } else {
        ""
    }
}

/// `range` of `source` as lines, each starting with `indentation` in place of the indentation the
/// first of them has in `source`, joined with `\n`, after `head`
fn reindented(source: &str, range: TextRange, indentation: &str, head: Option<&str>) -> String {
    let original = indentation_at(source, range.start());
    let mut text = String::new();
    for (index, line) in source[range].lines().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        let line = line.trim_end();
        text.push_str(indentation);
        if index == 0 {
            if let Some(head) = head {
                text.push_str(head);
            }
            text.push_str(line);
        } else {
            text.push_str(
                line.strip_prefix(original)
                    .unwrap_or_else(|| line.trim_start()),
            );
        }
    }
    text
}
