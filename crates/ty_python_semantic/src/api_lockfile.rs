//! api lockfile generator
//!
//! walks first-party modules and produces a single deterministic, line-oriented
//! summary of the public type-level api. the format is intentionally terse —
//! the lockfile is meant to be diffed, not parsed back into types. any
//! type-level change in a public symbol surfaces as a line-level diff
//!
//! the first line is `#api-lock:v=1` (grammar version). subsequent lines are
//! sorted lexicographically. one record per line:
//!
//! ```text
//! <qualified>:c[<bases>]                              # class
//! <qualified>:c<tv>[<bases>]                          # generic class
//! <qualified>:c[<bases>]{<flags>}                     # class with flags
//! <qualified>:d(<params>)-><ret>                      # def / method
//! <qualified>:d{<deco>}<tv>(<params>)-><ret>          # method with decorators / typevars
//! <qualified>:v=<type>                                # variable / class attribute
//! <qualified>:v[<quals>]=<type>                       # qualified variable
//! <qualified>:i=<type>                                # instance attribute (via self.x)
//! <qualified>:t=<type>                                # type alias
//! <qualified>:t<tv>=<type>                            # generic type alias
//! <qualified>:p=getter|setter|deleter,...             # property accessors present
//! <qualified>:r=<target>                              # re-export of class/function/alias
//! <qualified>:m=<module>                              # module re-export
//! ```
//!
//! `<tv>` is `<variance? name, ...>` with `+`/`-`/`*` variance prefixes
//! `<flags>` are class flags joined by `,`: `dataclass`, `enum`, `final`,
//!   `named_tuple`, `protocol`, `typed_dict`
//! `<deco>` are function decorators joined by `,`: `abstractmethod`, `async`,
//!   `classmethod`, `deprecated`, `final`, `no_type_check`, `overload`,
//!   `override`, `property`, `staticmethod`, `type_check_only`
//! `<quals>` are variable qualifiers joined by `,`: `classvar`, `final`,
//!   `initvar`, `notrequired`, `readonly`, `required`
use std::fmt::Write;

use ruff_db::files::File;
use ruff_python_ast::name::Name;
use rustc_hash::FxHashSet;
use ty_module_resolver::file_to_module;
use ty_python_core::{
    attribute_scopes, global_scope, place_table, scope::ScopeId, semantic_index, use_def_map,
};

use crate::Db;
use crate::dunder_all::dunder_all_names;
use crate::place::{place_from_bindings, place_from_declarations};
use crate::types::enums::is_enum_class;
use crate::types::function::{FunctionDecorators, FunctionType};
use crate::types::type_alias::TypeAliasType;
use crate::types::{
    ClassLiteral, KnownClass, KnownInstanceType, NominalInstanceType, Parameter, ParameterKind,
    PropertyInstanceType, Signature, SpecialFormType, Type, TypeQualifiers, TypeVarVariance,
    UnionType,
};

const FORMAT_HEADER: &str = "#api-lock:v=1";

/// module-level dunders that are conventionally part of a module's public api
const PUBLIC_MODULE_DUNDERS: &[&str] = &["__all__", "__author__", "__doc__", "__version__"];

/// generate the api lockfile contents for the given iterator of files
///
/// non-first-party files (stdlib, site-packages, vendored) are skipped — only
/// project code contributes to the lockfile.
///
/// `python_version` is recorded in the header so a downstream reader can
/// see what target the lockfile was generated against (typing constructs
/// like `Self`, `Required`, `NotRequired` resolve differently per
/// version)
pub fn generate_api_lockfile<'db, I>(db: &'db dyn Db, files: I, python_version: &str) -> String
where
    I: IntoIterator<Item = File>,
{
    use std::fmt::Write as _;

    let mut lines: Vec<String> = Vec::new();
    let mut visited_classes: FxHashSet<ClassLiteral<'db>> = FxHashSet::default();
    let mut module_count: usize = 0;

    for file in files {
        let Some(module) = file_to_module(db, file) else {
            continue;
        };

        let module_name = module.name(db).as_str().to_string();
        let scope = global_scope(db, file);
        emit_module_scope(db, scope, &module_name, &mut lines, &mut visited_classes);
        module_count += 1;
    }

    lines.sort();
    lines.dedup();

    let mut out = String::from(FORMAT_HEADER);
    out.push('\n');
    let _ = writeln!(out, "#tool:by={}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(out, "#python:{python_version}");
    let _ = writeln!(out, "#modules:{module_count}");
    out.push_str(&lines.join("\n"));
    out.push('\n');
    out
}

fn emit_module_scope<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    qualified_prefix: &str,
    lines: &mut Vec<String>,
    visited_classes: &mut FxHashSet<ClassLiteral<'db>>,
) {
    let use_def_map = use_def_map(db, scope);
    let table = place_table(db, scope);
    let scope_file = scope.file(db);
    let all_names = dunder_all_names(db, scope_file);

    for (symbol_id, declarations, bindings) in use_def_map.all_reachable_symbols() {
        let symbol = table.symbol(symbol_id);
        let name = symbol.name();
        if !is_public_module_symbol(name.as_str(), all_names.as_ref().map(|s| s as &_)) {
            continue;
        }

        let place_and_qualifiers =
            place_from_declarations(db, declarations).ignore_conflicting_declarations();
        let declaration_ty = place_and_qualifiers.place.ignore_possibly_undefined();
        let qualifiers = place_and_qualifiers.qualifiers;

        let binding_ty = place_from_bindings(db, bindings)
            .place
            .ignore_possibly_undefined();

        let Some(ty) = declaration_ty.or(binding_ty) else {
            continue;
        };

        // re-exports: classes/functions imported into a module are part of its
        // public api, but we don't want to repeat their member breakdown — emit
        // a one-line `r` record and let the defining module own the full body
        let owning_file = type_definition_file(db, ty);
        let qualified = format!("{qualified_prefix}.{name}");
        if let Some(owning) = owning_file
            && owning != scope_file
        {
            lines.push(format!("{qualified}:r={}", render_reexport(db, ty)));
            continue;
        }

        emit_symbol(db, &qualified, name, ty, qualifiers, lines, visited_classes);
    }
}

/// returns the file that defines the given type, if any. used to detect
/// re-exports in module scope
fn type_definition_file<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<File> {
    match ty {
        Type::ClassLiteral(class) => Some(class.file(db)),
        Type::GenericAlias(alias) => Some(ClassLiteral::Static(alias.origin(db)).file(db)),
        Type::FunctionLiteral(function) => Some(function.file(db)),
        Type::TypeAlias(TypeAliasType::PEP695(alias))
        | Type::KnownInstance(KnownInstanceType::TypeAliasType(TypeAliasType::PEP695(alias))) => {
            Some(alias.rhs_scope(db).file(db))
        }
        _ => None,
    }
}

fn emit_symbol<'db>(
    db: &'db dyn Db,
    qualified: &str,
    name: &Name,
    ty: Type<'db>,
    qualifiers: TypeQualifiers,
    lines: &mut Vec<String>,
    visited_classes: &mut FxHashSet<ClassLiteral<'db>>,
) {
    match ty {
        Type::ClassLiteral(class) => emit_class(db, qualified, name, class, lines, visited_classes),
        Type::GenericAlias(alias) => emit_class(
            db,
            qualified,
            name,
            ClassLiteral::Static(alias.origin(db)),
            lines,
            visited_classes,
        ),
        Type::FunctionLiteral(function) => emit_function(db, qualified, function, lines),
        Type::KnownInstance(KnownInstanceType::TypeAliasType(alias)) | Type::TypeAlias(alias) => {
            emit_type_alias(db, qualified, alias, lines);
        }
        Type::PropertyInstance(property) => emit_property(db, qualified, property, lines),
        Type::Union(union)
            if union
                .elements(db)
                .iter()
                .all(|t| matches!(t, Type::PropertyInstance(_))) =>
        {
            let merged = merge_property_union(db, union);
            emit_property(db, qualified, merged, lines);
        }
        Type::ModuleLiteral(module_lit) => {
            let target = module_lit.module(db).name(db).as_str();
            lines.push(format!("{qualified}:m={target}"));
        }
        _ => {
            lines.push(format!(
                "{qualified}:v{}={}",
                render_qualifiers(qualifiers),
                render_type(db, ty)
            ));
        }
    }
}

fn emit_class<'db>(
    db: &'db dyn Db,
    qualified: &str,
    name: &Name,
    class: ClassLiteral<'db>,
    lines: &mut Vec<String>,
    visited_classes: &mut FxHashSet<ClassLiteral<'db>>,
) {
    if !visited_classes.insert(class) {
        return;
    }

    // class header. bases render as bare class names; the lockfile reads
    // cleaner without surrounding `type[...]`
    let bases = match class {
        ClassLiteral::Static(static_class) => static_class
            .explicit_bases(db)
            .iter()
            .map(|base| render_class_base(db, *base))
            .collect::<Vec<_>>()
            .join(","),
        _ => String::new(),
    };
    let typevars = render_class_typevars(db, class);
    let flags = render_class_flags(db, class);
    lines.push(format!("{qualified}:c{typevars}[{bases}]{flags}"));

    // walk the class body for own members. stays out of inherited members so a
    // base-class change only diffs the base class line, not every subclass
    if let ClassLiteral::Static(static_class) = class {
        let body_scope = static_class.body_scope(db);
        let class_self_name = name.clone();
        emit_class_members(
            db,
            body_scope,
            qualified,
            &class_self_name,
            lines,
            visited_classes,
        );
        emit_instance_attributes(db, body_scope, qualified, lines);
    }
}

fn emit_class_members<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    qualified_prefix: &str,
    class_self_name: &Name,
    lines: &mut Vec<String>,
    visited_classes: &mut FxHashSet<ClassLiteral<'db>>,
) {
    let use_def_map = use_def_map(db, scope);
    let table = place_table(db, scope);

    for (symbol_id, declarations, bindings) in use_def_map.all_reachable_symbols() {
        let symbol = table.symbol(symbol_id);
        let name = symbol.name();
        if name == class_self_name {
            continue;
        }
        if !is_public_class_member(name.as_str()) {
            continue;
        }

        let place_and_qualifiers =
            place_from_declarations(db, declarations).ignore_conflicting_declarations();
        let declaration_ty = place_and_qualifiers.place.ignore_possibly_undefined();
        let qualifiers = place_and_qualifiers.qualifiers;

        let binding_ty = place_from_bindings(db, bindings)
            .place
            .ignore_possibly_undefined();

        let Some(ty) = declaration_ty.or(binding_ty) else {
            continue;
        };

        let qualified = format!("{qualified_prefix}.{name}");
        emit_symbol(db, &qualified, name, ty, qualifiers, lines, visited_classes);
    }
}

/// emit `:i=<type>` lines for instance attributes assigned via `self.x = ...`
/// inside the class's methods. resolves the type from declarations first, then
/// bindings as fallback
fn emit_instance_attributes<'db>(
    db: &'db dyn Db,
    body_scope: ScopeId<'db>,
    qualified_prefix: &str,
    lines: &mut Vec<String>,
) {
    let file = body_scope.file(db);
    let index = semantic_index(db, file);

    let mut seen: FxHashSet<String> = FxHashSet::default();
    for function_scope_id in attribute_scopes(db, body_scope) {
        let attr_table = index.place_table(function_scope_id);
        for member in attr_table.members() {
            let Some(name) = member.as_instance_attribute() else {
                continue;
            };
            if !is_public_class_member(name) {
                continue;
            }
            if !seen.insert(name.to_string()) {
                continue;
            }

            let Some((ty, qualifiers)) = lookup_instance_attribute(db, body_scope, name) else {
                continue;
            };
            let qualified = format!("{qualified_prefix}.{name}");
            lines.push(format!(
                "{qualified}:i{}={}",
                render_qualifiers(qualifiers),
                render_type(db, ty)
            ));
        }
    }
}

fn lookup_instance_attribute<'db>(
    db: &'db dyn Db,
    class_body_scope: ScopeId<'db>,
    name: &str,
) -> Option<(Type<'db>, TypeQualifiers)> {
    let file = class_body_scope.file(db);
    let index = semantic_index(db, file);

    // try declarations first across all attribute scopes
    for function_scope_id in attribute_scopes(db, class_body_scope) {
        let attr_table = index.place_table(function_scope_id);
        let Some(member) = attr_table.member_id_by_instance_attribute_name(name) else {
            continue;
        };
        let use_def = index.use_def_map(function_scope_id);
        let place_and_qualifiers =
            place_from_declarations(db, use_def.reachable_member_declarations(member))
                .ignore_conflicting_declarations();
        if let Some(ty) = place_and_qualifiers.place.ignore_possibly_undefined() {
            return Some((ty, place_and_qualifiers.qualifiers));
        }
    }

    // fall back to bindings (bindings carry no qualifiers)
    for function_scope_id in attribute_scopes(db, class_body_scope) {
        let attr_table = index.place_table(function_scope_id);
        let Some(member) = attr_table.member_id_by_instance_attribute_name(name) else {
            continue;
        };
        let use_def = index.use_def_map(function_scope_id);
        let binding = place_from_bindings(db, use_def.reachable_member_bindings(member));
        if let Some(ty) = binding.place.ignore_possibly_undefined() {
            return Some((ty, TypeQualifiers::empty()));
        }
    }

    None
}

fn emit_function<'db>(
    db: &'db dyn Db,
    qualified: &str,
    function: FunctionType<'db>,
    lines: &mut Vec<String>,
) {
    let decorators = render_function_decorators(db, function);
    for signature in function.signature(db) {
        let typevars = render_signature_typevars(db, signature);
        let body = render_signature_body(db, signature);
        lines.push(format!("{qualified}:d{decorators}{typevars}{body}"));
    }
}

fn emit_type_alias<'db>(
    db: &'db dyn Db,
    qualified: &str,
    alias: TypeAliasType<'db>,
    lines: &mut Vec<String>,
) {
    let typevars = match alias {
        TypeAliasType::PEP695(pep695) => pep695
            .generic_context(db)
            .map(|ctx| render_generic_context(db, ctx))
            .unwrap_or_default(),
        TypeAliasType::ManualPEP695(_) => String::new(),
    };
    lines.push(format!(
        "{qualified}:t{typevars}={}",
        render_type(db, alias.value_type(db))
    ));
}

/// merge a union of `PropertyInstance` types into one property by taking the
/// first non-`None` accessor for each slot. ty produces such unions when a
/// property is split across `@x.setter` / `@x.deleter` declarations
fn merge_property_union<'db>(db: &'db dyn Db, union: UnionType<'db>) -> PropertyInstanceType<'db> {
    let mut getter: Option<Type<'db>> = None;
    let mut setter: Option<Type<'db>> = None;
    let mut deleter: Option<Type<'db>> = None;
    for element in union.elements(db) {
        if let Type::PropertyInstance(p) = element {
            if getter.is_none() {
                getter = p.getter(db);
            }
            if setter.is_none() {
                setter = p.setter(db);
            }
            if deleter.is_none() {
                deleter = p.deleter(db);
            }
        }
    }
    PropertyInstanceType::new(db, getter, setter, deleter)
}

fn emit_property<'db>(
    db: &'db dyn Db,
    qualified: &str,
    property: PropertyInstanceType<'db>,
    lines: &mut Vec<String>,
) {
    let mut accessors: Vec<&'static str> = Vec::new();
    let mut return_ty: Option<Type<'db>> = None;
    if let Some(getter) = property.getter(db) {
        accessors.push("getter");
        if let Type::FunctionLiteral(function) = getter {
            return_ty = function
                .signature(db)
                .iter()
                .next()
                .map(|sig| sig.return_ty);
        }
    }
    if property.setter(db).is_some() {
        accessors.push("setter");
    }
    if property.deleter(db).is_some() {
        accessors.push("deleter");
    }
    let accessors_str = accessors.join(",");
    let type_str = return_ty
        .map(|ty| format!("={}", render_type(db, ty)))
        .unwrap_or_default();
    lines.push(format!("{qualified}:p[{accessors_str}]{type_str}"));
}

fn render_function_decorators<'db>(db: &'db dyn Db, function: FunctionType<'db>) -> String {
    let mut tokens: Vec<&'static str> = Vec::new();
    if function.has_known_decorator(db, FunctionDecorators::ABSTRACT_METHOD) {
        tokens.push("abstractmethod");
    }
    if is_async_function(db, function) {
        tokens.push("async");
    }
    if function.is_classmethod(db) {
        tokens.push("classmethod");
    }
    if function.implementation_deprecated(db).is_some() {
        tokens.push("deprecated");
    }
    if function.has_known_decorator(db, FunctionDecorators::FINAL) {
        tokens.push("final");
    }
    if function.has_known_decorator(db, FunctionDecorators::NO_TYPE_CHECK) {
        tokens.push("no_type_check");
    }
    if function.has_known_decorator(db, FunctionDecorators::OVERLOAD) {
        tokens.push("overload");
    }
    if function.has_known_decorator(db, FunctionDecorators::OVERRIDE) {
        tokens.push("override");
    }
    if function.is_staticmethod(db) {
        tokens.push("staticmethod");
    }
    if function.has_known_decorator(db, FunctionDecorators::TYPE_CHECK_ONLY) {
        tokens.push("type_check_only");
    }
    if tokens.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", tokens.join(","))
    }
}

/// detect async functions by inspecting whether the signature's return type is
/// `CoroutineType[...]` — `FunctionType::signature` wraps async return types in
/// this form
fn is_async_function<'db>(db: &'db dyn Db, function: FunctionType<'db>) -> bool {
    function
        .signature(db)
        .iter()
        .any(|sig| match sig.return_ty {
            Type::NominalInstance(instance) => {
                instance.has_known_class(db, KnownClass::CoroutineType)
            }
            _ => false,
        })
}

fn render_signature_typevars<'db>(db: &'db dyn Db, signature: &Signature<'db>) -> String {
    signature
        .generic_context
        .map(|ctx| render_generic_context(db, ctx))
        .unwrap_or_default()
}

fn render_generic_context<'db>(
    db: &'db dyn Db,
    context: crate::types::GenericContext<'db>,
) -> String {
    let mut out = String::from("<");
    let mut first = true;
    for bound_typevar in context.variables(db) {
        // `Self` is an implicit method-bound typevar injected by ty for instance
        // methods. it is not part of the user-visible api surface and would
        // otherwise pollute every method line with a `<Self>` prefix
        if bound_typevar.name(db).as_str() == "Self" {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        let sigil = match bound_typevar.variance(db) {
            TypeVarVariance::Covariant => "+",
            TypeVarVariance::Contravariant => "-",
            TypeVarVariance::Invariant => "",
            TypeVarVariance::Bivariant => "*",
        };
        write!(out, "{sigil}{}", bound_typevar.name(db).as_str()).unwrap();
    }
    if first {
        return String::new();
    }
    out.push('>');
    out
}

fn render_generic_args<'db>(db: &'db dyn Db, context: crate::types::GenericContext<'db>) -> String {
    let names: Vec<String> = context
        .variables(db)
        .map(|tv| tv.name(db).as_str().to_string())
        .collect();
    if names.is_empty() {
        String::new()
    } else {
        format!("[{}]", names.join(","))
    }
}

fn render_signature_body<'db>(db: &'db dyn Db, signature: &Signature<'db>) -> String {
    let parameters = signature.parameters();
    // ignore `self`/`cls`, which ty synthesises as positional-only — leaking
    // the `/` marker into the lockfile for every method is noise without
    // signal
    let is_real_pos_only = |p: &Parameter<'_>| match p.kind() {
        ParameterKind::PositionalOnly { name, .. } => match name {
            Some(n) => {
                let s = n.as_str();
                s != "self" && s != "cls"
            }
            None => true,
        },
        _ => false,
    };
    let any_positional_only = parameters.iter().any(is_real_pos_only);
    let any_explicit_variadic = parameters
        .iter()
        .any(|p| matches!(p.kind(), ParameterKind::Variadic { .. }));
    let any_keyword_only = parameters
        .iter()
        .any(|p| matches!(p.kind(), ParameterKind::KeywordOnly { .. }));

    let mut out = String::from("(");
    let mut tokens: Vec<String> = Vec::new();
    let mut emitted_pos_only_boundary = !any_positional_only;
    let mut emitted_kw_only_boundary = !any_keyword_only || any_explicit_variadic;
    let mut anon_index: usize = 0;

    for parameter in parameters {
        match parameter.kind() {
            ParameterKind::PositionalOnly { .. } if is_real_pos_only(parameter) => {}
            ParameterKind::PositionalOrKeyword { .. }
            | ParameterKind::Variadic { .. }
            | ParameterKind::KeywordOnly { .. }
            | ParameterKind::KeywordVariadic { .. }
                if !emitted_pos_only_boundary =>
            {
                tokens.push("/".to_string());
                emitted_pos_only_boundary = true;
            }
            _ => {}
        }

        if matches!(parameter.kind(), ParameterKind::KeywordOnly { .. })
            && !emitted_kw_only_boundary
        {
            tokens.push("*".to_string());
            emitted_kw_only_boundary = true;
        }

        let annotation = render_type(db, parameter.annotated_type());
        let token = match parameter.kind() {
            ParameterKind::PositionalOnly { name, default_type } => {
                let label = match name.as_ref() {
                    Some(n) => n.as_str().to_string(),
                    None => {
                        let label = format!("_{anon_index}");
                        anon_index += 1;
                        label
                    }
                };
                let mut s = format!("{label}:{annotation}");
                if default_type.is_some() {
                    s.push('=');
                }
                s
            }
            ParameterKind::PositionalOrKeyword { name, default_type }
            | ParameterKind::KeywordOnly { name, default_type } => {
                let mut s = format!("{name}:{annotation}");
                if default_type.is_some() {
                    s.push('=');
                }
                s
            }
            ParameterKind::Variadic { name } => {
                emitted_kw_only_boundary = true;
                format!("*{name}:{annotation}")
            }
            ParameterKind::KeywordVariadic { name } => {
                format!("**{name}:{annotation}")
            }
        };
        tokens.push(token);
    }

    out.push_str(&tokens.join(","));
    out.push(')');
    write!(out, "->{}", render_type(db, signature.return_ty)).unwrap();
    out
}

/// render a type to its lockfile form. nominal classes defined in user code
/// are prefixed with the defining module so the lockfile remains
/// disambiguated even if two modules export classes with the same short name.
/// unions and intersections are recursively walked so each member keeps its
/// module prefix
fn render_type<'db>(db: &'db dyn Db, ty: Type<'db>) -> String {
    if ty.is_none(db) {
        return "None".to_string();
    }
    // surface ty's `Unknown` as an obvious sentinel so a downstream diff
    // can distinguish a real api change from a checker regression
    if matches!(ty, Type::Dynamic(crate::types::DynamicType::Unknown)) {
        return "<unresolved>".to_string();
    }
    match ty {
        Type::NominalInstance(instance) => {
            let class_name = instance.class_name(db).as_str().to_owned();
            // anonymous named-tuple types: render structurally so two
            // identically-shaped anon NTs share a lockfile representation
            // and the synthesized hash-suffixed class name doesn't leak
            if class_name.starts_with("_AnonNamedTuple_") {
                if let Some(structural) = render_anon_named_tuple(db, instance) {
                    return structural;
                }
            }
            let module = instance
                .class_module_name(db)
                .map(|m| format!("{}.", m.as_str()))
                .unwrap_or_default();
            let base = format!("{module}{class_name}");
            // surface generic args so `list[int]` and `list[str]` don't both
            // collapse to `builtins.list` in the lockfile
            if let Some(alias) = instance.class(db).into_generic_alias() {
                let args: Vec<String> = alias
                    .specialization(db)
                    .types(db)
                    .iter()
                    .map(|arg| render_type(db, *arg))
                    .collect();
                if !args.is_empty() {
                    return format!("{base}[{}]", args.join(","));
                }
            }
            base
        }
        Type::ClassLiteral(class) => format!("type[{}]", qualified_class_name(db, class)),
        Type::GenericAlias(alias) => {
            let origin = qualified_class_name(db, ClassLiteral::Static(alias.origin(db)));
            let args: Vec<String> = alias
                .specialization(db)
                .types(db)
                .iter()
                .map(|arg| render_type(db, *arg))
                .collect();
            if args.is_empty() {
                origin
            } else {
                format!("{origin}[{}]", args.join(","))
            }
        }
        Type::Union(union) => {
            // group consecutive Literal members into a single `Literal[a, b, c]`
            // so unions of literal values stay compact in the lockfile
            let mut literal_parts: Vec<String> = Vec::new();
            let mut other_parts: Vec<String> = Vec::new();
            let mut has_none = false;
            for t in union.elements(db) {
                if t.is_none(db) {
                    has_none = true;
                } else if t.as_literal_value_kind().is_some() {
                    // peel the `Literal[...]` wrapper if the display added it
                    let rendered = render_type(db, *t);
                    let inner = rendered
                        .strip_prefix("Literal[")
                        .and_then(|s| s.strip_suffix(']'))
                        .map(ToOwned::to_owned)
                        .unwrap_or(rendered);
                    literal_parts.push(inner);
                } else {
                    other_parts.push(render_type(db, *t));
                }
            }
            literal_parts.sort();
            other_parts.sort();
            let mut elements: Vec<String> = Vec::new();
            if !literal_parts.is_empty() {
                elements.push(format!("Literal[{}]", literal_parts.join(", ")));
            }
            elements.extend(other_parts);
            // None is canonically last for readability of `T | None` style
            if has_none {
                elements.push("None".to_owned());
            }
            elements.join(" | ")
        }
        Type::TypeVar(bound) => bound.name(db).as_str().to_string(),
        Type::KnownInstance(KnownInstanceType::SubscriptedGeneric(ctx)) => {
            format!("typing.Generic{}", render_generic_args(db, ctx))
        }
        Type::KnownInstance(KnownInstanceType::SubscriptedProtocol(ctx)) => {
            format!("typing.Protocol{}", render_generic_args(db, ctx))
        }
        Type::SpecialForm(form) => form.to_string(),
        Type::Intersection(intersection) => {
            let mut positives: Vec<String> = intersection
                .iter_positive(db)
                .map(|t| render_type(db, t))
                .collect();
            let mut negatives: Vec<String> = intersection
                .iter_negative(db)
                .map(|t| render_type(db, t))
                .collect();
            positives.sort();
            negatives.sort();
            let mut parts: Vec<String> = Vec::new();
            parts.extend(positives);
            parts.extend(negatives.into_iter().map(|n| format!("~{n}")));
            parts.join(" & ")
        }
        _ => ty.display(db).to_string(),
    }
}

/// Render the structural shape of an anonymous named-tuple instance as
/// `(name: T, name: T, ...)`. Walks the class's body-scope declarations
/// in source order to recover field names. Falls back to a `tuple[T, …]`
/// shape if no named fields are visible (synthesised anon NTs from `.by`
/// source may not expose body declarations to ty).
fn render_anon_named_tuple<'db>(
    db: &'db dyn Db,
    instance: NominalInstanceType<'db>,
) -> Option<String> {
    let class_literal = instance.class(db).class_literal(db);
    if let ClassLiteral::Static(static_class) = class_literal {
        let body_scope = static_class.body_scope(db);
        let table = place_table(db, body_scope);
        let use_def = use_def_map(db, body_scope);
        let mut fields: Vec<(String, String)> = Vec::new();
        for (symbol_id, declarations, _bindings) in use_def.all_reachable_symbols() {
            let symbol = table.symbol(symbol_id);
            let name = symbol.name().as_str().to_owned();
            if name.starts_with('_') {
                continue;
            }
            let place_and_qualifiers =
                place_from_declarations(db, declarations).ignore_conflicting_declarations();
            let Some(decl_ty) = place_and_qualifiers.place.ignore_possibly_undefined() else {
                continue;
            };
            fields.push((name, render_type(db, decl_ty)));
        }
        if !fields.is_empty() {
            let body = fields
                .iter()
                .map(|(n, t)| format!("{n}: {t}"))
                .collect::<Vec<_>>()
                .join(", ");
            return Some(format!("({body})"));
        }
    }
    // fall back to tuple-spec elements when body declarations aren't
    // available
    let spec = instance.tuple_spec(db)?;
    let elements: Vec<&Type<'db>> = spec.fixed_elements().collect();
    if elements.is_empty() {
        return None;
    }
    let body = elements
        .iter()
        .map(|t| render_type(db, **t))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("({body})"))
}

fn render_reexport<'db>(db: &'db dyn Db, ty: Type<'db>) -> String {
    match ty {
        Type::ClassLiteral(class) => qualified_class_name(db, class),
        Type::GenericAlias(alias) => {
            let origin = qualified_class_name(db, ClassLiteral::Static(alias.origin(db)));
            let args: Vec<String> = alias
                .specialization(db)
                .types(db)
                .iter()
                .map(|arg| render_type(db, *arg))
                .collect();
            if args.is_empty() {
                origin
            } else {
                format!("{origin}[{}]", args.join(","))
            }
        }
        Type::FunctionLiteral(function) => {
            let module = file_to_module(db, function.file(db))
                .map(|m| format!("{}.", m.name(db).as_str()))
                .unwrap_or_default();
            format!("{module}{}", function.name(db).as_str())
        }
        Type::TypeAlias(TypeAliasType::PEP695(alias))
        | Type::KnownInstance(KnownInstanceType::TypeAliasType(TypeAliasType::PEP695(alias))) => {
            let file = alias.rhs_scope(db).file(db);
            let module = file_to_module(db, file)
                .map(|m| format!("{}.", m.name(db).as_str()))
                .unwrap_or_default();
            format!("{module}{}", alias.name(db))
        }
        _ => render_type(db, ty),
    }
}

/// render the generic typevar list for a class, including variance.
/// returns empty string for non-generic classes
fn render_class_typevars<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> String {
    let Some(generic_context) = class.generic_context(db) else {
        return String::new();
    };
    render_generic_context(db, generic_context)
}

fn render_class_flags<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> String {
    let mut flags: Vec<&'static str> = Vec::new();
    if is_dataclass(db, class) {
        flags.push("dataclass");
    }
    if matches!(class, ClassLiteral::DynamicEnum(_)) || is_enum_class(db, Type::ClassLiteral(class))
    {
        flags.push("enum");
    }
    if class.is_final(db) {
        flags.push("final");
    }
    if is_named_tuple(db, class) {
        flags.push("named_tuple");
    }
    if class.is_protocol(db) {
        flags.push("protocol");
    }
    if class.is_typed_dict(db) {
        flags.push("typed_dict");
    }
    if flags.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", flags.join(","))
    }
}

fn is_dataclass<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> bool {
    match class {
        ClassLiteral::Static(static_class) => static_class.is_dataclass_like(db),
        ClassLiteral::Dynamic(_)
        | ClassLiteral::DynamicNamedTuple(_)
        | ClassLiteral::DynamicTypedDict(_)
        | ClassLiteral::DynamicEnum(_) => false,
    }
}

fn is_named_tuple<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> bool {
    match class {
        ClassLiteral::DynamicNamedTuple(_) => true,
        ClassLiteral::Static(static_class) => static_class
            .explicit_bases(db)
            .iter()
            .any(|base| matches!(base, Type::SpecialForm(SpecialFormType::NamedTuple))),
        _ => false,
    }
}

fn render_class_base<'db>(db: &'db dyn Db, ty: Type<'db>) -> String {
    match ty {
        Type::ClassLiteral(class) => qualified_class_name(db, class),
        Type::GenericAlias(alias) => {
            let origin = qualified_class_name(db, ClassLiteral::Static(alias.origin(db)));
            let args: Vec<String> = alias
                .specialization(db)
                .types(db)
                .iter()
                .map(|arg| render_type(db, *arg))
                .collect();
            if args.is_empty() {
                origin
            } else {
                format!("{origin}[{}]", args.join(","))
            }
        }
        _ => render_type(db, ty),
    }
}

fn qualified_class_name<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> String {
    let class_name = class.name(db).as_str();
    let class_file = class.file(db);
    if let Some(module) = file_to_module(db, class_file) {
        format!("{}.{}", module.name(db).as_str(), class_name)
    } else {
        class_name.to_string()
    }
}

fn render_qualifiers(qualifiers: TypeQualifiers) -> String {
    if qualifiers.is_empty() {
        return String::new();
    }
    let mut tokens: Vec<&'static str> = Vec::new();
    if qualifiers.contains(TypeQualifiers::CLASS_VAR) {
        tokens.push("classvar");
    }
    if qualifiers.contains(TypeQualifiers::FINAL) {
        tokens.push("final");
    }
    if qualifiers.contains(TypeQualifiers::INIT_VAR) {
        tokens.push("initvar");
    }
    if qualifiers.contains(TypeQualifiers::NOT_REQUIRED) {
        tokens.push("notrequired");
    }
    if qualifiers.contains(TypeQualifiers::READ_ONLY) {
        tokens.push("readonly");
    }
    if qualifiers.contains(TypeQualifiers::REQUIRED) {
        tokens.push("required");
    }
    if tokens.is_empty() {
        String::new()
    } else {
        format!("[{}]", tokens.join(","))
    }
}

/// module-level symbols filter:
/// - if `__all__` is set, only names in it are public
/// - otherwise, names without leading underscore are public, plus an
///   allowlist of public-by-convention dunders (`__version__`, etc.)
fn is_public_module_symbol(name: &str, all_names: Option<&FxHashSet<Name>>) -> bool {
    if let Some(names) = all_names {
        return names.iter().any(|n| n.as_str() == name);
    }
    if PUBLIC_MODULE_DUNDERS.contains(&name) {
        return true;
    }
    !name.starts_with('_')
}

/// class members are public unless name-mangled (single underscore) or
/// strictly private (`__name` without trailing dunder). standard dunders like
/// `__init__` are part of the public api surface and stay
fn is_public_class_member(name: &str) -> bool {
    if name.starts_with("__") && name.ends_with("__") && name.len() >= 4 {
        return true;
    }
    !name.starts_with('_')
}
