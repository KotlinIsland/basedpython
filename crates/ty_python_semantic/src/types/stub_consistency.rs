//! basedpython: where a runtime module and the stub that describes it disagree
//!
//! the checks run as the runtime module is inferred, one per declaration the stub pairs with
//! (see [`crate::stub_mapping`]): a function's parameter list and annotations, a class's bases, a
//! variable's annotation, and for the module and each class both declare, the members only one of
//! them declares. the stub's types are compared as the runtime module reads them (see
//! `stub_implementation`), and one that names something only the stub has, which means nothing in
//! the runtime module, is passed over.

use ruff_db::diagnostic::{Annotation, Span};
use ruff_db::parsed::parsed_module;
use ruff_python_ast as ast;
use ruff_python_ast::helpers::is_dunder;
use ruff_python_ast::name::Name;
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::definition::{Definition, DefinitionKind, DefinitionState};
use ty_python_core::place::ScopedPlaceId;
use ty_python_core::scope::{ScopeId, ScopeKind};
use ty_python_core::{ProgramFile, global_scope, place_table, semantic_index};

use crate::dunder_all::dunder_all_names;
use crate::place::Place;
use crate::stub_mapping::{
    is_stub_only_by_design, runtime_definitions, runtime_module_stub, stub_scope,
};
use crate::types::class_base::ClassBase;
use crate::types::context::InferContext;
use crate::types::diagnostic::{
    INCOMPATIBLE_STUB_ATTRIBUTE, INCOMPATIBLE_STUB_BASES, INCOMPATIBLE_STUB_SIGNATURE,
    MISSING_RUNTIME_MEMBER, MISSING_STUB_MEMBER,
};
use crate::types::infer::original_class_type;
use crate::types::signatures::{Parameter, ReturnCallableTypeVarScope};
use crate::types::stub_implementation::{StubSignature, stub_declared_types, stub_signatures};
use crate::types::{
    ClassLiteral, ProgramEnvironment, Type, UnionType, infer_definition_types, inferred_declaration,
};
use crate::{Db, attribute_declarations};

/// a span leading to a declaration of the stub
fn stub_span(db: &dyn Db, definition: Definition<'_>) -> Span {
    let module = parsed_module(db, definition.program_file(db).python_file(db)).load(db);
    Span::from(definition.focus_range(db, &module))
}

/// the members of the runtime scope `scope` the stub declares and the scope does not define, and,
/// where `missing-stub-member` is enabled, its public members the stub does not declare
///
/// `range` is where a member of the scope is reported: a class's name, or the start of the
/// module's first statement for the module's own members, which have nowhere better to point
pub(crate) fn check_scope_members<'db>(
    context: &InferContext<'db, '_>,
    scope: ScopeId<'db>,
    class: Option<ClassLiteral<'db>>,
    range: TextRange,
) {
    let db = context.db();
    let Some(stub_scope) = stub_scope(db, scope) else {
        return;
    };
    let env = ProgramEnvironment::from_scope(scope);
    let table = place_table(db, scope);
    let container = match class {
        Some(class) => format!("class `{}`", class.name(db)),
        None => "this module".to_string(),
    };

    if context.is_lint_enabled(&MISSING_RUNTIME_MEMBER) {
        // a module's `__getattr__` answers for any name it does not bind; a class's is found by
        // the member lookup below
        let module_getattr = class.is_none()
            && table
                .symbol_by_name("__getattr__")
                .is_some_and(ty_python_core::symbol::Symbol::is_bound);
        for member in &stub_scope.members {
            if !member.runtime.is_empty()
                || module_getattr
                || is_stub_only_by_design(db, &member.name, &member.stub)
            {
                continue;
            }
            // a class may have the member by inheriting it, or have it from a class-level
            // `__getattr__`, neither of which its own body shows
            if let Some(class) = class {
                let instance = Type::instance(db, &env, class.default_specialization(db));
                let found = |ty: Type<'db>| !ty.member(db, &env, &member.name).place.is_undefined();
                if found(instance) || found(Type::ClassLiteral(class)) {
                    continue;
                }
            }
            let Some(builder) = context.report_lint(&MISSING_RUNTIME_MEMBER, range) else {
                continue;
            };
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "`{name}` is declared by the stub but not defined by {container}",
                name = member.name,
            ));
            if let Some(&first) = member.stub.first() {
                diagnostic
                    .annotate(Annotation::secondary(stub_span(db, first)).message("declared here"));
            }
        }
    }

    if context.is_lint_enabled(&MISSING_STUB_MEMBER) {
        let program_file = scope.program_file(db);
        // where a module lists what it exports, what it leaves out of the list is not public
        let dunder_all = class
            .is_none()
            .then(|| dunder_all_names(db, program_file))
            .flatten();
        for (name, definition) in &stub_scope.runtime_only {
            // a type variable or an alias is typing's machinery in a module as in a stub
            if is_dunder(name) || is_stub_only_by_design(db, name, std::slice::from_ref(definition))
            {
                continue;
            }
            let listed = dunder_all.map(|names| names.contains(name));
            if listed == Some(false) {
                continue;
            }
            if definition.kind(db).is_import() && listed != Some(true) {
                continue;
            }
            let module = parsed_module(db, program_file.python_file(db)).load(db);
            let Some(builder) = context.report_lint(
                &MISSING_STUB_MEMBER,
                definition.focus_range(db, &module).range(),
            ) else {
                continue;
            };
            builder.into_diagnostic(format_args!(
                "`{name}` is public but not declared by the stub of {container}"
            ));
        }
    }
}

/// the members of the runtime module the stub declares and the module does not define
pub(crate) fn check_module<'db>(context: &InferContext<'db, '_>, file: ProgramFile<'db>) {
    let db = context.db();
    if runtime_module_stub(db, file).is_none() {
        return;
    }
    // a member the module lacks has no place in it of its own; the start of the module's first
    // statement stands for the module
    let module = parsed_module(db, file.python_file(db)).load(db);
    let start = module
        .suite()
        .first()
        .map_or_else(TextRange::default, |first| TextRange::empty(first.start()));
    check_scope_members(context, global_scope(db, file), None, start);
}

/// a runtime class against the stub's declaration of it: its bases and its members
pub(crate) fn check_class<'db>(
    context: &InferContext<'db, '_>,
    definition: Definition<'db>,
    class_node: &ast::StmtClassDef,
) {
    let db = context.db();
    let Some(stub_declarations) =
        crate::stub_mapping::stub_counterpart_declarations(db, definition).as_deref()
    else {
        return;
    };
    let Some(runtime_class) = original_class_type(db, definition) else {
        return;
    };
    let index = semantic_index(db, definition.program_file(db));
    if let Some(body) =
        index.try_node_scope(ty_python_core::scope::NodeWithScopeRef::Class(class_node))
    {
        let body = body.to_scope_id(db, definition.program_file(db));
        check_scope_members(context, body, Some(runtime_class), class_node.name.range());
        check_instance_attributes(context, body);
    }

    // a decorator may replace the class with another whose bases the class statement does not
    // show: `enum._simple_enum(StrEnum)` builds a `StrEnum` out of a plain class, and is typed as
    // handing back the class it is given
    if !context.is_lint_enabled(&INCOMPATIBLE_STUB_BASES) || !class_node.decorator_list.is_empty() {
        return;
    }
    let runtime_mro: Vec<ClassLiteral<'db>> = runtime_class
        .iter_mro(db)
        .filter_map(|base| match base {
            ClassBase::Class(class) => Some(class.class_literal(db)),
            _ => None,
        })
        .collect();
    let Some(stub_file) = runtime_module_stub(db, definition.program_file(db)) else {
        return;
    };
    for &stub_declaration in stub_declarations {
        let Some(stub_class) = original_class_type(db, stub_declaration) else {
            continue;
        };
        for base in stub_class.explicit_bases(db).iter().copied() {
            let base_class = match base {
                Type::ClassLiteral(class) => class,
                Type::GenericAlias(alias) => ClassLiteral::Static(alias.origin(db)),
                _ => continue,
            };
            if base_class.is_protocol(db) {
                continue;
            }
            // a base the stub defines is the runtime module's class of the same name there
            let accepted: Vec<ClassLiteral<'db>> = match base_class.definition(db) {
                Some(base_definition) if base_definition.file(db) == stub_file.file(db) => {
                    runtime_definitions(db, base_definition, None)
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|runtime| original_class_type(db, runtime))
                        .collect()
                }
                _ => vec![base_class],
            };
            if accepted.is_empty() || accepted.iter().any(|class| runtime_mro.contains(class)) {
                continue;
            }
            let Some(builder) =
                context.report_lint(&INCOMPATIBLE_STUB_BASES, class_node.name.range())
            else {
                continue;
            };
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "Class `{class}` does not inherit from `{base}`, which its stub declares as a base",
                class = class_node.name,
                base = base_class.name(db),
            ));
            diagnostic.annotate(
                Annotation::secondary(stub_span(db, stub_declaration))
                    .message("the stub's declaration"),
            );
        }
    }
}

/// how a parameter of a stub's signature fails to be accepted by the runtime function
enum ParameterMismatch {
    /// nothing in the runtime function takes the argument
    Missing,
    /// the runtime function takes it, but not by position
    NotPositional,
    /// the runtime function takes it, but not by keyword
    NotKeyword,
    /// the stub lets a call leave it out and the runtime function does not
    NoDefault,
}

/// the runtime parameter a call through the stub's `stub_parameter`, at `position` among the
/// stub's parameters past the receiver, passes its argument to, and how it fails to take it
///
/// the parameter is answered along with the mismatch where there is one, since a call through the
/// stub still reaches it
fn match_parameter<'a, 'db>(
    stub_parameter: &Parameter<'db>,
    position: usize,
    runtime: &[&'a Parameter<'db>],
) -> (Option<&'a Parameter<'db>>, Option<ParameterMismatch>) {
    let positional = || {
        runtime
            .get(position)
            .copied()
            .filter(|parameter| parameter.is_positional())
            .or_else(|| runtime.iter().copied().find(|p| p.is_variadic()))
    };
    let by_name = |name: &Name| {
        runtime
            .iter()
            .copied()
            .find(|parameter| parameter.keyword_name() == Some(name))
            .or_else(|| runtime.iter().copied().find(|p| p.is_keyword_variadic()))
    };
    let found = if stub_parameter.is_variadic() {
        runtime.iter().copied().find(|p| p.is_variadic())
    } else if stub_parameter.is_keyword_variadic() {
        runtime.iter().copied().find(|p| p.is_keyword_variadic())
    } else if stub_parameter.is_positional_only() {
        positional()
    } else if let Some(name) = stub_parameter.keyword_name() {
        let named = by_name(name);
        if stub_parameter.is_keyword_only() {
            named
        } else {
            match (positional(), named) {
                (None, None) => None,
                (Some(at_position), None) => {
                    return (Some(at_position), Some(ParameterMismatch::NotKeyword));
                }
                (None, Some(named)) => {
                    return (Some(named), Some(ParameterMismatch::NotPositional));
                }
                (Some(at_position), Some(named)) => {
                    // a parameter of another name in the position takes the argument passed by
                    // position, so the stub's name does not reach the same parameter both ways
                    if !std::ptr::eq(at_position, named)
                        && !at_position.is_variadic()
                        && !named.is_keyword_variadic()
                    {
                        return (Some(at_position), Some(ParameterMismatch::NotKeyword));
                    }
                    Some(named)
                }
            }
        }
    } else {
        return (None, None);
    };
    let Some(found) = found else {
        return (None, Some(ParameterMismatch::Missing));
    };
    if stub_parameter.has_default()
        && !found.has_default()
        && !found.is_variadic()
        && !found.is_keyword_variadic()
    {
        return (Some(found), Some(ParameterMismatch::NoDefault));
    }
    (Some(found), None)
}

/// the parameter of `node` written for `parameter` of its signature, where the source has one
fn parameter_node<'a>(
    node: &'a ast::StmtFunctionDef,
    parameter: &Parameter<'_>,
) -> Option<ast::AnyParameterRef<'a>> {
    let name = parameter.name()?;
    node.parameters
        .iter()
        .find(|candidate| candidate.name().as_str() == name.as_str())
}

/// a runtime function against the stub's declarations of it: whether it takes every call each of
/// them accepts, and whether the annotations it writes agree with theirs
pub(crate) fn check_function<'db>(
    context: &InferContext<'db, '_>,
    definition: Definition<'db>,
    node: &ast::StmtFunctionDef,
) {
    let db = context.db();
    if !context.is_lint_enabled(&INCOMPATIBLE_STUB_SIGNATURE) {
        return;
    }
    let Some(stub_signatures) = stub_signatures(db, definition).as_deref() else {
        return;
    };
    let Some(function) = infer_definition_types(db, definition).function_type(definition) else {
        return;
    };
    let env = ProgramEnvironment::from_file(definition.program_file(db));
    let overload = function.literal(db).last_definition;
    // the runtime module's own `@overload`s declare signatures of their own; the implementation
    // is what has to take every call the stub accepts
    if overload.is_overload(db) {
        return;
    }
    let signature = overload.parameter_signature(db, ReturnCallableTypeVarScope::Public);
    let binds_receiver = overload.binds_first_parameter(db);
    let runtime: Vec<&Parameter<'db>> = signature
        .parameters()
        .iter()
        .skip(usize::from(binds_receiver))
        .collect();
    let name = &node.name;

    let report = |range: TextRange, stub_signature: &StubSignature<'db>, message: String| {
        if let Some(builder) = context.report_lint(&INCOMPATIBLE_STUB_SIGNATURE, range) {
            let mut diagnostic = builder.into_diagnostic(message);
            diagnostic.annotate(
                Annotation::secondary(stub_signature.overload.spans(db).signature)
                    .message("the stub's declaration"),
            );
        }
    };

    // each runtime parameter some call through the stub reaches, so that one no call reaches is
    // one every call leaves out
    let mut reached: Vec<Vec<bool>> = Vec::new();
    for stub_signature in stub_signatures {
        let mut reached_here = vec![false; runtime.len()];
        let stub_parameters = stub_signature
            .signature
            .parameters()
            .iter()
            .skip(usize::from(stub_signature.binds_receiver));
        for (position, stub_parameter) in stub_parameters.enumerate() {
            let stub_name = stub_parameter
                .name()
                .map_or_else(|| format!("{}", position + 1), ToString::to_string);
            let (found, mismatch) = match_parameter(stub_parameter, position, &runtime);
            if let Some(found) = found
                && let Some(index) = runtime.iter().position(|p| std::ptr::eq(*p, found))
            {
                reached_here[index] = true;
            }
            if let Some(mismatch) = mismatch {
                let message = match mismatch {
                    ParameterMismatch::Missing => {
                        format!("`{name}` has no parameter for the stub's parameter `{stub_name}`")
                    }
                    ParameterMismatch::NotPositional => format!(
                        "Parameter `{stub_name}` of `{name}` cannot be passed by position, \
                         as its stub declares"
                    ),
                    ParameterMismatch::NotKeyword => format!(
                        "Parameter `{stub_name}` of `{name}` cannot be passed by keyword, \
                         as its stub declares"
                    ),
                    ParameterMismatch::NoDefault => format!(
                        "Parameter `{stub_name}` of `{name}` has no default, \
                         but its stub lets a call leave it out"
                    ),
                };
                let range = found
                    .and_then(|found| parameter_node(node, found))
                    .map_or(node.parameters.range(), |p| p.range());
                report(range, stub_signature, message);
                continue;
            }
            let Some(found) = found else {
                continue;
            };
            // an annotation the runtime function writes has to accept what the stub says the
            // parameter is passed
            let stub_ty = stub_parameter.annotated_type();
            if !found.inferred_annotation
                && !stub_parameter.inferred_annotation
                && !found.is_variadic()
                && !found.is_keyword_variadic()
                && !stub_parameter.is_variadic()
                && !stub_parameter.is_keyword_variadic()
                && !stub_ty.is_assignable_to(db, &env, found.annotated_type())
                && let Some(parameter) = parameter_node(node, found)
                && let Some(annotation) = parameter.annotation()
            {
                report(
                    annotation.range(),
                    stub_signature,
                    format!(
                        "Parameter `{parameter}` of `{name}` is annotated `{runtime_ty}`, \
                         which does not accept the stub's `{stub_ty}`",
                        parameter = parameter.name(),
                        runtime_ty = found.annotated_type().display(db, &env),
                        stub_ty = stub_ty.display(db, &env),
                    ),
                );
            }
        }
        reached.push(reached_here);
    }

    for (index, parameter) in runtime.iter().enumerate() {
        if parameter.has_default() || parameter.is_variadic() || parameter.is_keyword_variadic() {
            continue;
        }
        let Some(signature_index) = reached.iter().position(|reached| !reached[index]) else {
            continue;
        };
        let Some(written) = parameter_node(node, parameter) else {
            continue;
        };
        report(
            written.range(),
            &stub_signatures[signature_index],
            format!(
                "Parameter `{parameter}` of `{name}` is required, \
                 but a call its stub accepts leaves it out",
                parameter = written.name(),
            ),
        );
    }

    // a return type the runtime function writes has to promise no more than the stub's
    if let Some(returns) = node.returns.as_deref()
        && !node.is_asserts_return
    {
        let mut stub_returns = Vec::with_capacity(stub_signatures.len());
        for stub_signature in stub_signatures {
            if stub_signature.is_async != node.is_async {
                return;
            }
            let Some(awaited) = stub_signature.awaited else {
                return;
            };
            stub_returns.push(awaited);
        }
        let stub_return = UnionType::from_elements(db, &env, stub_returns);
        let runtime_return = overload
            .raw_signature(db, ReturnCallableTypeVarScope::Public)
            .return_ty;
        if !runtime_return.is_assignable_to(db, &env, stub_return) {
            report(
                returns.range(),
                &stub_signatures[0],
                format!(
                    "`{name}` returns `{runtime_return}`, which is not assignable to \
                     the stub's return type `{stub_return}`",
                    runtime_return = runtime_return.display(db, &env),
                    stub_return = stub_return.display(db, &env),
                ),
            );
        }
    }
}

/// a module variable or class attribute annotated in the runtime module against the type the stub
/// declares for it
pub(crate) fn check_variable<'db>(
    context: &InferContext<'db, '_>,
    definition: Definition<'db>,
    annotation: &ast::Expr,
    runtime_ty: Type<'db>,
) {
    let db = context.db();
    if !context.is_lint_enabled(&INCOMPATIBLE_STUB_ATTRIBUTE) {
        return;
    }
    let file = definition.program_file(db);
    if runtime_module_stub(db, file).is_none() {
        return;
    }
    let ScopedPlaceId::Symbol(symbol) = definition.place(db) else {
        return;
    };
    let index = semantic_index(db, file);
    let file_scope = definition.file_scope(db);
    if !matches!(
        index.scope(file_scope).kind(),
        ScopeKind::Module | ScopeKind::Class
    ) {
        return;
    }
    let name = index.place_table(file_scope).symbol(symbol).name();
    check_declared_type(
        context,
        file_scope.to_scope_id(db, file),
        name,
        runtime_ty,
        annotation.range(),
    );
}

/// the attributes the methods of a runtime class declare through `self`, against the types the
/// stub declares for them in the class body
fn check_instance_attributes<'db>(context: &InferContext<'db, '_>, class_body: ScopeId<'db>) {
    let db = context.db();
    if !context.is_lint_enabled(&INCOMPATIBLE_STUB_ATTRIBUTE) {
        return;
    }
    let Some(declared) = stub_declared_types(db, class_body).as_ref() else {
        return;
    };
    let Some(stub_scope) = stub_scope(db, class_body).as_ref() else {
        return;
    };
    let module = parsed_module(db, class_body.program_file(db).python_file(db)).load(db);
    // in the stub's order, so that the diagnostics are in an order that does not change
    let names = stub_scope
        .members
        .iter()
        .map(|member| &member.name)
        .filter(|name| declared.contains_key(*name));
    for name in names {
        for (declarations, _) in attribute_declarations(db, class_body, name) {
            for declaration in declarations {
                let DefinitionState::Defined(definition) = declaration.declaration else {
                    continue;
                };
                let DefinitionKind::AnnotatedAssignment(assignment) = definition.kind(db) else {
                    continue;
                };
                let Some(runtime) = inferred_declaration(db, definition).declared() else {
                    continue;
                };
                check_declared_type(
                    context,
                    class_body,
                    name,
                    runtime.inner_type(),
                    assignment.annotation(&module).range(),
                );
            }
        }
    }
}

/// the type the runtime module declares for `name` in `scope`, annotated at `range`, against the
/// type the stub declares for it there
fn check_declared_type<'db>(
    context: &InferContext<'db, '_>,
    scope: ScopeId<'db>,
    name: &Name,
    runtime_ty: Type<'db>,
    range: TextRange,
) {
    let db = context.db();
    let Some(declared) = stub_declared_types(db, scope)
        .as_ref()
        .and_then(|declared| declared.get(name))
    else {
        return;
    };
    let Place::Defined(stub_place) = declared.place else {
        return;
    };
    let env = ProgramEnvironment::from_scope(scope);
    let stub_ty = stub_place.ty;
    // importers read the stub's type from the variable and may assign it values of that type
    if runtime_ty.is_assignable_to(db, &env, stub_ty)
        && stub_ty.is_assignable_to(db, &env, runtime_ty)
    {
        return;
    }
    let Some(builder) = context.report_lint(&INCOMPATIBLE_STUB_ATTRIBUTE, range) else {
        return;
    };
    let mut diagnostic = builder.into_diagnostic(format_args!(
        "`{name}` is declared as `{runtime_ty}` here but as `{stub_ty}` in the stub",
        runtime_ty = runtime_ty.display(db, &env),
        stub_ty = stub_ty.display(db, &env),
    ));
    if let Some(stub_scope) = stub_scope(db, scope).as_ref()
        && let Some(member) = stub_scope.member(name)
        && let Some(&first) = member.stub.iter().find(|definition| {
            matches!(definition.kind(db), DefinitionKind::AnnotatedAssignment(_))
        })
    {
        diagnostic.annotate(Annotation::secondary(stub_span(db, first)).message("declared here"));
    }
}
