//! basedpython extension declarations (`extension list:`)
//!
//! an extension adds methods and computed properties to an existing type
//! without subclassing it. the parser lowers the block to a [`ClassDef`]
//! carrying a synthetic `extension_def` marker; the semantic index binds it
//! under a mangled `<extension:…>` symbol so the extended type's name is never
//! shadowed. this module answers the two questions the checker and the
//! transpiler share:
//!
//! - which extensions are applicable in a file (its own plus those of every
//!   module it imports with a plain `import mod`)
//! - what an attribute access on a receiver resolves to when normal member
//!   lookup finds nothing (`xs.second()` on `list[int]` → the extension
//!   method, specialized by the receiver)
//!
//! the extended type's own type parameters are reused by name inside an
//! extension body (`Element` on `list`), so a member signature is written in
//! terms of the extended class's typevars — applying the receiver's
//! specialization is then the ordinary generic-member substitution. a
//! bracketed bound (`extension list[Element: int]`) re-declares that name as a
//! *constrained* typevar for the body and narrows where the extension applies;
//! it never introduces a parameter the extended type does not declare
//!
//! [`ClassDef`]: ruff_python_ast::StmtClassDef

use ruff_db::files::File;
use ruff_python_ast as ast;
use ty_module_resolver::resolve_module;
use ty_python_core::{global_scope, place_table, semantic_index};

use crate::Db;
use crate::place::{builtins_symbol, global_symbol};
use crate::types::Type;
use crate::types::class::{ClassLiteral, ClassType, StaticClassLiteral};
use crate::types::class_base::ClassBase;
use crate::types::context::InferContext;
use crate::types::diagnostic::INVALID_EXTENSION;
use crate::types::member::class_member;
use crate::types::typevar::{BoundTypeVarInstance, TypeVarBoundOrConstraints};

/// the symbol-name prefix the semantic index gives extension declarations
pub(crate) const EXTENSION_SYMBOL_PREFIX: &str = "<extension:";

/// all extension declarations in a module, in source order
#[salsa::tracked(returns(deref), heap_size = ruff_memory_usage::heap_size)]
pub(crate) fn extensions_in_module(db: &dyn Db, file: File) -> Box<[StaticClassLiteral<'_>]> {
    // only basedpython files declare extensions. a `.py` file containing an
    // `extension` block already has a parse error; don't serve its members
    if !file.source_type(db).is_basedpython() {
        return Box::default();
    }
    let global = global_scope(db, file);
    let mut extensions = Vec::new();
    for symbol in place_table(db, global).symbols() {
        if !symbol.name().starts_with(EXTENSION_SYMBOL_PREFIX) {
            continue;
        }
        let Some(Type::ClassLiteral(ClassLiteral::Static(candidate))) =
            class_member(db, global, symbol.name()).ignore_possibly_undefined()
        else {
            continue;
        };
        if candidate.is_extension(db) {
            extensions.push(candidate);
        }
    }
    extensions.into_boxed_slice()
}

/// the extensions applicable in `file`: its own, then those of every module it
/// imports with a plain `import mod` (in that order — a same-module extension
/// wins over an imported one when both apply)
#[salsa::tracked(returns(deref), heap_size = ruff_memory_usage::heap_size)]
pub(crate) fn applicable_extensions(db: &dyn Db, file: File) -> Box<[StaticClassLiteral<'_>]> {
    if !file.source_type(db).is_basedpython() {
        return Box::default();
    }
    let mut extensions: Vec<StaticClassLiteral<'_>> = extensions_in_module(db, file).to_vec();
    for module_name in semantic_index(db, file).imported_modules() {
        let Some(module) = resolve_module(db, file, module_name) else {
            continue;
        };
        let Some(module_file) = module.file(db) else {
            continue;
        };
        if module_file == file {
            continue;
        }
        extensions.extend_from_slice(extensions_in_module(db, module_file));
    }
    extensions.into_boxed_slice()
}

/// the class an extension declaration extends: its name resolved in the
/// declaring module's globals, else builtins. `None` when the name does not
/// resolve to a class (reported at the declaration)
#[salsa::tracked]
pub(crate) fn extended_class<'db>(
    db: &'db dyn Db,
    extension: StaticClassLiteral<'db>,
) -> Option<ClassLiteral<'db>> {
    let name = extension.name(db);
    let file = extension.file(db);
    let resolved = global_symbol(db, file, name)
        .place
        .ignore_possibly_undefined()
        .or_else(|| builtins_symbol(db, name).place.ignore_possibly_undefined())?;
    let literal = resolved.as_class_literal()?;
    // an extension of an extension makes no sense; the mangled binding makes
    // this unreachable in practice, but be explicit
    if let ClassLiteral::Static(static_literal) = literal
        && static_literal.is_extension(db)
    {
        return None;
    }
    Some(literal)
}

/// the extension body's view of the extended class: specialized at the
/// bracket-declared (bounded) typevar where one is spelled, else at the
/// extended class's own typevar. this is the implicit type of `self` in the
/// extension's methods
pub(crate) fn body_view_class<'db>(
    db: &'db dyn Db,
    extension: StaticClassLiteral<'db>,
) -> Option<ClassType<'db>> {
    let target = extended_class(db, extension)?;
    let Some(target_context) = target.generic_context(db) else {
        return Some(ClassType::NonGeneric(target));
    };
    let extension_context = extension.generic_context(db);
    let types: Vec<Type<'db>> = target_context
        .variables(db)
        .map(|target_var| {
            let spelled = extension_context.and_then(|context| {
                context.binds_named_typevar(db, target_var.typevar(db).name(db))
            });
            Type::TypeVar(spelled.unwrap_or(target_var))
        })
        .collect();
    Some(target.apply_specialization(db, |context| context.specialize(db, types)))
}

/// resolve `name` inside an extension body to the extended class's own typevar
/// of that name. bracket-spelled typevars resolve normally through the
/// type-param scope before this fallback is consulted, so this only serves the
/// unconstrained remainder
pub(crate) fn extension_body_typevar<'db>(
    db: &'db dyn Db,
    extension: StaticClassLiteral<'db>,
    name: &str,
) -> Option<BoundTypeVarInstance<'db>> {
    let target = extended_class(db, extension)?;
    target
        .generic_context(db)?
        .variables(db)
        .find(|variable| variable.typevar(db).name(db).as_str() == name)
}

/// what kind of member an extension attribute resolved to. drives the
/// transpiler's call-site rewrite (a property call drops the parentheses; a
/// classmethod receives the class object)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionMemberKind {
    Method,
    Property,
    StaticMethod,
    ClassMethod,
}

/// how the transpiler rewrites an attribute access that resolved to an
/// extension member
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionAttributeInfo {
    /// the module-level backing function the member lowers to
    pub function: String,
    pub kind: ExtensionMemberKind,
    /// the module to import the backing function from, when the extension is
    /// declared in a module other than the one being transpiled
    pub import_from: Option<String>,
    /// whether the receiver is the class object itself (`str.parse(…)`) rather
    /// than an instance — decides what a `class def` member receives
    pub receiver_is_class: bool,
}

/// the backing-function name an extension member lowers to:
/// `__by_ext__list__second`. when a module declares more than one extension
/// of the same target name, later ones carry an ordinal (`__by_ext2__…`) so
/// their members do not collide. the transpiler's block lowering computes the
/// same name from the extension file's AST alone
pub(crate) fn backing_function_name<'db>(
    db: &'db dyn Db,
    extension: StaticClassLiteral<'db>,
    member: &str,
) -> String {
    let target = extension.name(db);
    let ordinal = extensions_in_module(db, extension.file(db))
        .iter()
        .filter(|candidate| candidate.name(db) == target)
        .position(|candidate| *candidate == extension)
        .unwrap_or(0);
    if ordinal == 0 {
        format!("__by_ext__{target}__{member}")
    } else {
        format!("__by_ext{}__{target}__{member}", ordinal + 1)
    }
}

/// a successful extension-member resolution
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExtensionMemberResolution<'db> {
    /// the extension declaration that supplied the member
    pub(crate) extension: StaticClassLiteral<'db>,
    /// the type of the attribute expression: the member bound against the
    /// receiver (a bound method, a property's value, …)
    pub(crate) ty: Type<'db>,
    pub(crate) kind: ExtensionMemberKind,
    /// another applicable extension that also supplies the member — an
    /// ambiguity the checker reports at the access site
    pub(crate) ambiguous_with: Option<StaticClassLiteral<'db>>,
}

/// resolve an attribute access that found no regular member: consult the
/// extensions applicable in `file` for one that extends the receiver's class
/// (or a class in its MRO), whose bracket bounds the receiver's specialization
/// satisfies, and that declares `name`. extensions never shadow declared
/// members — the caller only asks after normal lookup came up undefined
pub(crate) fn resolve_extension_member<'db>(
    db: &'db dyn Db,
    file: File,
    receiver: Type<'db>,
    name: &str,
) -> Option<ExtensionMemberResolution<'db>> {
    let candidates = applicable_extensions(db, file);
    if candidates.is_empty() {
        return None;
    }

    // an instance receiver serves every member kind; a class-object receiver
    // serves only `static def` / `class def` members
    let (receiver_class, instance) = if let Some(class) = receiver.nominal_class(db) {
        (class, Some(receiver))
    } else if let Some(class) = receiver.to_class_type(db) {
        (class, None)
    } else {
        return None;
    };

    let mut resolved: Option<ExtensionMemberResolution<'db>> = None;
    for &extension in candidates {
        let Some(member) = applicable_member(db, extension, receiver_class, name) else {
            continue;
        };
        let kind = classify_member(db, member);
        if instance.is_none()
            && !matches!(
                kind,
                ExtensionMemberKind::StaticMethod | ExtensionMemberKind::ClassMethod
            )
        {
            continue;
        }
        match &mut resolved {
            None => {
                let owner = match instance {
                    Some(instance_ty) => instance_ty.to_meta_type(db),
                    None => receiver,
                };
                let bound = member
                    .try_call_dunder_get(db, instance, owner)
                    .map(|(ty, _)| ty)
                    .unwrap_or(member);
                resolved = Some(ExtensionMemberResolution {
                    extension,
                    ty: bound,
                    kind,
                    ambiguous_with: None,
                });
            }
            Some(resolution) => {
                if resolution.ambiguous_with.is_none() {
                    resolution.ambiguous_with = Some(extension);
                }
            }
        }
    }
    resolved
}

/// if `extension` extends `receiver_class` (or a class in its MRO), its bounds
/// hold for the receiver's specialization, and it declares `name`, return the
/// member's type with the receiver's type arguments substituted in
fn applicable_member<'db>(
    db: &'db dyn Db,
    extension: StaticClassLiteral<'db>,
    receiver_class: ClassType<'db>,
    name: &str,
) -> Option<Type<'db>> {
    let target = extended_class(db, extension)?;

    // find the extended class in the receiver's MRO, with the specialization
    // the receiver gives it — extension methods are inherited like any others
    let target_class = receiver_class.iter_mro(db).find_map(|base| match base {
        ClassBase::Class(class) if class.class_literal(db) == target => Some(class),
        _ => None,
    })?;
    let receiver_specialization = match target_class {
        ClassType::Generic(alias) => Some(alias.specialization(db)),
        ClassType::NonGeneric(_) => None,
    };

    // bracket bounds constrain the receiver: every spelled typevar must name a
    // parameter the extended class declares, and the receiver's argument for
    // it must satisfy the bound
    let extension_context = extension.generic_context(db);
    let mut extension_substitution: Vec<Type<'db>> = Vec::new();
    if let Some(extension_context) = extension_context {
        let target_context = target.generic_context(db)?;
        for extension_var in extension_context.variables(db) {
            let target_var =
                target_context.binds_named_typevar(db, extension_var.typevar(db).name(db))?;
            let receiver_argument = receiver_specialization
                .and_then(|specialization| specialization.get(db, target_var))
                .unwrap_or_else(Type::unknown);
            if !satisfies_bound(db, receiver_argument, extension_var) {
                return None;
            }
            extension_substitution.push(receiver_argument);
        }
    }

    let member = class_member(db, extension.body_scope(db), name).ignore_possibly_undefined()?;

    // substitute the receiver's type arguments: first for the extension's own
    // bracket typevars (matched to the target's parameters by name), then for
    // the extended class's typevars reused directly by the member's signature
    let mut member = member;
    if let Some(extension_context) = extension_context {
        member = member
            .apply_specialization(db, extension_context.specialize(db, extension_substitution));
    }
    member = member.apply_optional_specialization(db, receiver_specialization);
    Some(member)
}

/// does the receiver's type argument satisfy a bracket typevar's bound?
fn satisfies_bound<'db>(
    db: &'db dyn Db,
    argument: Type<'db>,
    extension_var: BoundTypeVarInstance<'db>,
) -> bool {
    match extension_var.typevar(db).bound_or_constraints(db) {
        None => true,
        Some(TypeVarBoundOrConstraints::UpperBound(bound)) => argument.is_assignable_to(db, bound),
        Some(TypeVarBoundOrConstraints::Constraints(constraints)) => constraints
            .elements(db)
            .iter()
            .any(|constraint| argument.is_assignable_to(db, *constraint)),
    }
}

/// declaration-site validation, run from the post-inference static-class
/// checks: the extended name must resolve to a class, bracket params must
/// reuse (and only constrain) parameters the extended type declares, and the
/// body must add behaviour, not state
pub(crate) fn validate_extension_declaration<'db>(
    context: &InferContext<'db, '_>,
    extension: StaticClassLiteral<'db>,
    class_node: &ast::StmtClassDef,
) {
    let db = context.db();

    let Some(target) = extended_class(db, extension) else {
        if let Some(builder) = context.report_lint(&INVALID_EXTENSION, &class_node.name) {
            builder.into_diagnostic(format_args!(
                "`{}` is not a class; an extension must name an existing class",
                class_node.name,
            ));
        }
        return;
    };

    if let Some(type_params) = class_node.type_params.as_deref() {
        let target_context = target.generic_context(db);
        for param in &type_params.type_params {
            let name = param.name();
            let declared = matches!(param, ast::TypeParam::TypeVar(_))
                && target_context.is_some_and(|target_context| {
                    target_context
                        .variables(db)
                        .any(|variable| variable.typevar(db).name(db).as_str() == name.as_str())
                });
            if !declared && let Some(builder) = context.report_lint(&INVALID_EXTENSION, param) {
                builder.into_diagnostic(format_args!(
                    "`{}` declares no type parameter `{name}`; an extension \
                    reuses the extended type's own parameters by name",
                    target.name(db),
                ));
            }
        }
    }

    for stmt in &class_node.body {
        let invalid = match stmt {
            ast::Stmt::Assign(_) | ast::Stmt::AnnAssign(_) | ast::Stmt::AugAssign(_) => {
                Some("stored fields")
            }
            ast::Stmt::ClassDef(_) => Some("nested classes"),
            _ => None,
        };
        if let Some(what) = invalid
            && let Some(builder) = context.report_lint(&INVALID_EXTENSION, stmt)
        {
            builder.into_diagnostic(format_args!(
                "an extension adds methods and computed properties; {what} are not allowed",
            ));
        }
    }
}

fn classify_member<'db>(db: &'db dyn Db, member: Type<'db>) -> ExtensionMemberKind {
    match member {
        Type::FunctionLiteral(function) if function.is_staticmethod(db) => {
            ExtensionMemberKind::StaticMethod
        }
        Type::FunctionLiteral(function) if function.is_classmethod(db) => {
            ExtensionMemberKind::ClassMethod
        }
        Type::PropertyInstance(_) => ExtensionMemberKind::Property,
        _ => ExtensionMemberKind::Method,
    }
}
