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
use ty_module_resolver::{ModuleName, resolve_module};
use ty_python_core::{global_scope, place_table, semantic_index};

use crate::Db;
use crate::place::{builtins_symbol, global_symbol};
use crate::types::ProgramEnvironment;
use crate::types::call::CallArguments;
use crate::types::class::{ClassLiteral, ClassType, KnownClass, StaticClassLiteral};
use crate::types::class_base::ClassBase;
use crate::types::conformance;
use crate::types::context::InferContext;
use crate::types::conversions::CONVERSION_DUNDERS;
use crate::types::diagnostic::INVALID_EXTENSION;
use crate::types::generics::Specialization;
use crate::types::member::class_member;
use crate::types::typevar::{BoundTypeVarInstance, TypeVarBoundOrConstraints};
use crate::types::{MemberLookupPolicy, Type};
use ty_module_resolver::ImportingFile;

/// the symbol-name prefix the semantic index gives extension declarations
pub(crate) const EXTENSION_SYMBOL_PREFIX: &str = "<extension:";

/// the vendored basedpython prelude — a `.byi` stub of builtin `extension`
/// declarations (the grapheme string surface, and the frozen containers'
/// `__of__`) every basedpython file sees without importing. its members are
/// type-only: the transpiler lowers each access to a plain python expression, so
/// the generic extension-call rewrite skips them (see [`is_prelude_extension`])
const PRELUDE_MODULE: &str = "ty_extensions._prelude";

/// the prelude module's file, resolved from `from_file`'s search paths. `None`
/// when the vendored stub is unavailable
pub(crate) fn prelude_file(db: &dyn Db, from_file: File) -> Option<File> {
    let name = ModuleName::new_static(PRELUDE_MODULE)?;
    resolve_module(
        db,
        ImportingFile::File(
            from_file,
            db.program_file(from_file).resolver_environment(db),
        ),
        &name,
    )?
    .file(db)
}

/// whether `extension` is declared in the basedpython prelude. the transpiler
/// asks so it can leave a prelude member's access to the lowering that group has
/// — construction for a conversion dunder, a plain expression for the grapheme
/// string surface — rather than emitting a backing-function call
pub(crate) fn is_prelude_extension(
    db: &dyn Db,
    from_file: File,
    extension: StaticClassLiteral<'_>,
) -> bool {
    prelude_file(db, from_file).is_some_and(|prelude| prelude == extension.file(db))
}

/// all extension declarations in a module, in source order
///
/// resolving a declaration to its class literal infers module-level code, and
/// that inference asks which extensions apply — so this query can re-enter
/// itself. it starts a cycle from "no extensions" and iterates: a member
/// lookup made while the set is still being discovered simply does not see one
#[salsa::tracked(
    returns(deref),
    cycle_initial = |_, _, _| Box::default(),
    heap_size = ruff_memory_usage::heap_size
)]
pub(crate) fn extensions_in_module(db: &dyn Db, file: File) -> Box<[StaticClassLiteral<'_>]> {
    // only basedpython files declare extensions. a `.py` file containing an
    // `extension` block already has a parse error; don't serve its members
    if !file.source_type(db).is_basedpython() {
        return Box::default();
    }
    let global = global_scope(db, db.program_file(file));
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
/// imports (in that order — a same-module extension wins over an imported one
/// when both apply).
///
/// Both `import mod` and `from mod import X` count. Naming what a module
/// declares is how a file most often depends on it — a conformance is written
/// against an interface imported by name — and requiring a separate `import mod`
/// whose symbols are never used would leave an import that reads as removable to
/// anyone tidying the file, silently withdrawing every member it carried.
///
/// cycles for the same reason [`extensions_in_module`] does, and recovers the
/// same way
#[salsa::tracked(
    returns(deref),
    cycle_initial = |_, _, _| Box::default(),
    heap_size = ruff_memory_usage::heap_size
)]
pub(crate) fn applicable_extensions(db: &dyn Db, file: File) -> Box<[StaticClassLiteral<'_>]> {
    if !file.source_type(db).is_basedpython() {
        return Box::default();
    }
    let mut extensions: Vec<StaticClassLiteral<'_>> = extensions_in_module(db, file).to_vec();
    // `imported_modules` deliberately records only `import mod` (see its docs),
    // so the `from mod import X` forms are collected from the file's own statements
    let imported = semantic_index(db, db.program_file(file))
        .imported_modules()
        .chain(crate::types::conversions::from_imported_modules(db, file));
    for module_name in imported {
        let Some(module) = resolve_module(
            db,
            ImportingFile::File(file, db.program_file(file).resolver_environment(db)),
            module_name,
        ) else {
            continue;
        };
        let Some(module_file) = module.file(db) else {
            continue;
        };
        if module_file == file {
            continue;
        }
        for &extension in extensions_in_module(db, module_file) {
            if !extensions.contains(&extension) {
                extensions.push(extension);
            }
        }
    }
    // the builtin prelude applies everywhere without an import, so it is folded
    // in last — a same-module or imported extension of the same member wins
    if let Some(prelude) = prelude_file(db, file)
        && prelude != file
    {
        for &extension in extensions_in_module(db, prelude) {
            if !extensions.contains(&extension) {
                extensions.push(extension);
            }
        }
    }
    extensions.into_boxed_slice()
}

/// the classes an applicable extension supplies a conversion dunder for.
///
/// The transpiler's hot-path gate asks whether a type might convert before doing
/// the work to find out; an extension-supplied dunder is invisible to a member
/// lookup on the class, so it has to be discovered from the extension side. The
/// prelude puts three classes here and a file rarely adds more, so the answer is
/// a short list rather than a set
#[salsa::tracked(returns(deref), heap_size = ruff_memory_usage::heap_size)]
pub(crate) fn conversion_extension_targets(db: &dyn Db, file: File) -> Box<[ClassLiteral<'_>]> {
    let mut targets: Vec<ClassLiteral<'_>> = Vec::new();
    for &extension in applicable_extensions(db, file) {
        let Some(target) = extended_class(db, extension) else {
            continue;
        };
        if targets.contains(&target) {
            continue;
        }
        let body = extension.body_scope(db);
        if CONVERSION_DUNDERS
            .iter()
            .any(|dunder| !class_member(db, body, dunder).is_undefined())
        {
            targets.push(target);
        }
    }
    targets.into_boxed_slice()
}

/// does an applicable `extension` supply a conversion dunder for `class`, or for
/// anything it inherits from?
///
/// The transpiler's gate must not miss one, or it skips a site the checker
/// accepted and emits python that never converts. Tracked for the same reason
/// `class_declares_conversion` is: the gate asks it of every argument and every
/// parameter of every call, and the prelude keeps the target list non-empty in
/// every file, so the MRO walk would otherwise run on all of them
#[salsa::tracked(returns(copy), heap_size = ruff_memory_usage::heap_size)]
pub(crate) fn extension_converts_class<'db>(
    db: &'db dyn Db,
    file: File,
    class: StaticClassLiteral<'db>,
) -> bool {
    let converting = conversion_extension_targets(db, file);
    !converting.is_empty()
        && class.default_specialization(db).iter_mro(db).any(|base| {
            base.into_class()
                .is_some_and(|base| converting.contains(&base.class_literal(db)))
        })
}

/// the class an extension declaration extends: its name resolved in the
/// declaring module's globals, else builtins. `None` when the name does not
/// resolve to a class (reported at the declaration)
///
/// resolving the name infers module-level code, and that inference asks which
/// conformances are visible — which asks what every extension extends. the cycle
/// starts from "not resolved yet" and iterates, exactly as the queries either
/// side of it do
#[salsa::tracked(returns(copy), cycle_initial = |_, _, _| None)]
pub(crate) fn extended_class<'db>(
    db: &'db dyn Db,
    extension: StaticClassLiteral<'db>,
) -> Option<ClassLiteral<'db>> {
    let env = &ProgramEnvironment::from_file(extension.program_file(db));
    let name = extension.name(db);
    let file = extension.file(db);
    let resolved = global_symbol(db, db.program_file(file), name)
        .place
        .ignore_possibly_undefined()
        .or_else(|| {
            builtins_symbol(db, env, name)
                .place
                .ignore_possibly_undefined()
        })?;
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
    /// a `static let` computed property: read off the class, like a `class def`
    /// member, but without call parentheses
    StaticProperty,
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
/// `_by_ext__list__second`. when a module declares more than one extension
/// of the same target name, later ones carry an ordinal (`_by_ext2__…`) so
/// their members do not collide. the transpiler's block lowering computes the
/// same name from the extension file's AST alone.
///
/// exactly one leading underscore: python private-name-mangles any `__name`
/// reference inside a class body, so a two-underscore name would break an
/// extension call written in one
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
        format!("_by_ext__{target}__{member}")
    } else {
        format!("_by_ext{}__{target}__{member}", ordinal + 1)
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
    env: &ProgramEnvironment<'db>,
    file: File,
    receiver: Type<'db>,
    name: &str,
) -> Option<ExtensionMemberResolution<'db>> {
    let mut resolutions = resolve_extension_members(db, env, file, receiver, name).into_iter();
    let mut resolved = resolutions.next()?;
    resolved.ambiguous_with = resolutions.next().map(|other| other.extension);
    Some(resolved)
}

/// every applicable extension that supplies `name` for `receiver`, in
/// precedence order.
///
/// [`resolve_extension_member`] collapses this to the winner plus a flag; a
/// caller that has to *describe* the losers — a conversion site reporting which
/// extensions disagree — needs each one's own resolution
pub(crate) fn resolve_extension_members<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    receiver: Type<'db>,
    name: &str,
) -> Vec<ExtensionMemberResolution<'db>> {
    let candidates = applicable_extensions(db, file);
    if candidates.is_empty() {
        return Vec::new();
    }

    // a use-site modifier says nothing about which class the receiver is an
    // instance of, so a `final Box` reaches `Box`'s extensions
    let receiver = receiver.erase_restriction(db);

    // an instance receiver serves every member kind; a class-object receiver
    // serves only `static def` / `class def` members
    let (receiver_class, instance) = if let Some(class) = receiver.nominal_class(db, env) {
        (class, Some(receiver))
    } else {
        // `type[C]` is a class object just as much as `C` itself is, so it
        // reaches the same `class def` and `static def` members. `to_class_type`
        // does not cover it — it answers only for a class written literally —
        // so a `type[…]` receiver is unwrapped to the class it is a subclass of
        let class_object = match receiver {
            Type::SubclassOf(subclass_of) => subclass_of.subclass_of().into_class(db, env),
            _ => receiver.to_class_type(db),
        };
        match class_object {
            Some(class) => (class, None),
            None => return Vec::new(),
        }
    };

    // a member reached through a conformance is written against the interface,
    // so it binds against the interface: the value really is one, and its own
    // class is not in the interface's lattice
    let bind = |member: Type<'db>, conformed_as: Option<ClassType<'db>>| {
        let bind_instance = match (conformed_as, instance) {
            (Some(protocol), Some(_)) => Some(Type::instance(db, env, protocol)),
            (_, other) => other,
        };
        let owner = match bind_instance {
            Some(instance_ty) => instance_ty.to_meta_type(db, env),
            None => receiver,
        };
        member
            .try_call_dunder_get(db, env, bind_instance, owner)
            .ok()
            .flatten()
            .map_or(member, |result| result.return_type)
    };

    let mut resolved = Vec::new();
    for &extension in candidates {
        let Some(applicable) = applicable_member(db, env, file, extension, receiver_class, name)
        else {
            continue;
        };
        let member = applicable.member;
        let kind = classify_member(db, member);
        if instance.is_none()
            && !matches!(
                kind,
                ExtensionMemberKind::StaticMethod
                    | ExtensionMemberKind::ClassMethod
                    | ExtensionMemberKind::StaticProperty
            )
        {
            continue;
        }
        resolved.push(ExtensionMemberResolution {
            extension,
            ty: bind(member, applicable.conformed_as),
            kind,
            ambiguous_with: None,
        });
    }

    // a conformance's own member *overrides* a default the interface's own
    // extension supplies, rather than competing with it — which is how the
    // witness table resolves it, and reporting an ambiguity instead made the
    // documented override flow unusable. dropping the overridden candidates here
    // rather than folding pairwise leaves every remaining one a genuine peer, so
    // a caller that has to describe the losers can take them at face value
    if resolved
        .iter()
        .any(|resolution| is_conformance(db, resolution.extension))
    {
        resolved.retain(|resolution| is_conformance(db, resolution.extension));
    }
    resolved
}

/// does `extension` declare a conformance, rather than plain members?
fn is_conformance<'db>(db: &'db dyn Db, extension: StaticClassLiteral<'db>) -> bool {
    !conformance::declared_conformances(db, extension).is_empty()
}

/// basedpython: the extension method `name` supplies for `receiver`, bound to
/// it, when the receiver's own type has no such member.
///
/// An operator never goes through attribute lookup — `+x` calls `__pos__` on
/// the meta-type directly — so operator inference asks here instead of through
/// the attribute fallback. The precedence is the same one every extension
/// member follows: a declared dunder wins, and an extension only answers what
/// nothing else does.
pub(crate) fn extension_operator<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    receiver: Type<'db>,
    name: &str,
) -> Option<ExtensionMemberResolution<'db>> {
    // the same lookup the operator itself performs — on the meta-type, with no
    // instance fallback — so an extension answers exactly where that finds
    // nothing
    if !receiver
        .member_lookup_with_policy(db, env, name, MemberLookupPolicy::NO_INSTANCE_FALLBACK)
        .place
        .is_undefined()
    {
        return None;
    }
    let resolution = resolve_extension_member(db, env, file, receiver, name)?;
    // an operator is invoked, so only a method can answer one — a computed
    // property named `__pos__` would be read, not called
    matches!(resolution.kind, ExtensionMemberKind::Method).then_some(resolution)
}

/// basedpython: an operator an extension supplies the dunder for, and which
/// operand it is called on.
///
/// The checker and the transpiler both resolve an operator through here, so the
/// call the checker approved is the call the lowering emits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExtensionOperator<'db> {
    pub(crate) resolution: ExtensionMemberResolution<'db>,
    /// the dunder the operator resolved to
    pub(crate) member: &'static str,
    /// whether the *right* operand is the receiver — a reflected binary
    /// operator (`__radd__`) or a membership test (`a in b` calls
    /// `b.__contains__(a)`)
    pub(crate) reflected: bool,
}

impl<'db> ExtensionOperator<'db> {
    /// the type the operator evaluates to. `None` when the resolved member
    /// does not accept the other operand, which leaves the operator
    /// unsupported exactly as it is without the extension
    pub(crate) fn return_type(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        arguments: &CallArguments<'_, 'db>,
    ) -> Option<Type<'db>> {
        self.resolution
            .ty
            .try_call(db, env, arguments)
            .ok()
            .map(|bindings| bindings.return_type(db, env))
    }
}

/// The extension supplying `op`'s dunder for a unary operator, if any.
pub(crate) fn unary_extension_operator<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    op: ast::UnaryOp,
    operand: Type<'db>,
) -> Option<ExtensionOperator<'db>> {
    let dunder = match op {
        ast::UnaryOp::Invert => "__invert__",
        ast::UnaryOp::UAdd => "__pos__",
        ast::UnaryOp::USub => "__neg__",
        // `not` reads truthiness rather than calling an operator dunder, and
        // the basedpython postfix operators are lowered before any dunder
        ast::UnaryOp::Not
        | ast::UnaryOp::Optional
        | ast::UnaryOp::Propagate
        | ast::UnaryOp::Force => return None,
    };
    Some(ExtensionOperator {
        resolution: extension_operator(db, env, file, operand, dunder)?,
        member: dunder,
        reflected: false,
    })
}

/// The extension supplying `op`'s dunder for a binary operator, if any. The
/// left operand's own dunder is tried first, then the right operand's
/// reflected one — the order python itself resolves in.
pub(crate) fn binary_extension_operator<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    left: Type<'db>,
    op: ast::Operator,
    right: Type<'db>,
) -> Option<ExtensionOperator<'db>> {
    extension_operator(db, env, file, left, op.dunder())
        .map(|resolution| ExtensionOperator {
            resolution,
            member: op.dunder(),
            reflected: false,
        })
        .or_else(|| {
            extension_operator(db, env, file, right, op.reflected_dunder()).map(|resolution| {
                ExtensionOperator {
                    resolution,
                    member: op.reflected_dunder(),
                    reflected: true,
                }
            })
        })
}

/// The extension supplying `op`'s dunder for a comparison, if any. A
/// membership test resolves against the *right* operand — `a in b` calls
/// `b.__contains__(a)` — and an identity test has no dunder at all.
pub(crate) fn comparison_extension_operator<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    left: Type<'db>,
    op: ast::CmpOp,
    right: Type<'db>,
) -> Option<ExtensionOperator<'db>> {
    let (reflected, dunder) = match op {
        ast::CmpOp::In | ast::CmpOp::NotIn => (true, "__contains__"),
        ast::CmpOp::Eq => (false, "__eq__"),
        ast::CmpOp::NotEq => (false, "__ne__"),
        ast::CmpOp::Lt => (false, "__lt__"),
        ast::CmpOp::LtE => (false, "__le__"),
        ast::CmpOp::Gt => (false, "__gt__"),
        ast::CmpOp::GtE => (false, "__ge__"),
        ast::CmpOp::Is | ast::CmpOp::IsNot => return None,
    };
    let receiver = if reflected { right } else { left };
    Some(ExtensionOperator {
        resolution: extension_operator(db, env, file, receiver, dunder)?,
        member: dunder,
        reflected,
    })
}

/// how an extension applies to one receiver: the type arguments its members'
/// signatures must be specialized with
pub(crate) struct ExtensionApplication<'db> {
    /// the specialization the receiver gives the extended class, which the
    /// extension's members reuse by name
    receiver_specialization: Option<Specialization<'db>>,
    /// the receiver's argument for each bracket-declared typevar, in the
    /// extension's own parameter order. empty when the extension declares none
    bracket_substitution: Vec<Type<'db>>,
}

impl<'db> ExtensionApplication<'db> {
    /// substitute this application's type arguments into a member's type: first
    /// the extension's own bracket typevars (matched to the extended type's
    /// parameters by name), then the extended class's typevars the member's
    /// signature reuses directly
    fn apply(
        &self,
        db: &'db dyn Db,
        extension: StaticClassLiteral<'db>,
        member: Type<'db>,
    ) -> Type<'db> {
        let mut member = member;
        if let Some(context) = extension.generic_context(db) {
            member = member.apply_specialization(
                db,
                context.specialize(db, self.bracket_substitution.clone()),
            );
        }
        member.apply_optional_specialization(db, self.receiver_specialization)
    }
}

/// does `extension` apply to a receiver of `receiver_class` — is the extended
/// class in the receiver's MRO, and does the receiver satisfy every bracket
/// bound?
pub(crate) fn extension_applies<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    extension: StaticClassLiteral<'db>,
    receiver_class: ClassType<'db>,
) -> Option<ExtensionApplication<'db>> {
    let target = extended_class(db, extension)?;

    // find the extended class in the receiver's MRO, with the specialization
    // the receiver gives it — extension methods are inherited like any others
    let target_class = receiver_class.iter_mro(db).find_map(|base| match base {
        ClassBase::Class(class) if class.class_literal(db) == target => Some(class),
        _ => None,
    })?;
    applied_at(db, env, extension, target, target_class)
}

/// [`extension_applies`] once the extended class has been located, with the
/// specialization the receiver gives it
pub(crate) fn applied_at<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    extension: StaticClassLiteral<'db>,
    target: ClassLiteral<'db>,
    target_class: ClassType<'db>,
) -> Option<ExtensionApplication<'db>> {
    let receiver_specialization = match target_class {
        ClassType::Generic(alias) => Some(alias.specialization(db)),
        ClassType::NonGeneric(_) => None,
    };

    // bracket bounds constrain the receiver: every spelled typevar must name a
    // parameter the extended class declares, and the receiver's argument for
    // it must satisfy the bound
    let mut bracket_substitution: Vec<Type<'db>> = Vec::new();
    if let Some(extension_context) = extension.generic_context(db) {
        let target_context = target.generic_context(db)?;
        for extension_var in extension_context.variables(db) {
            let target_var =
                target_context.binds_named_typevar(db, extension_var.typevar(db).name(db))?;
            let receiver_argument = receiver_specialization
                .and_then(|specialization| specialization.get(db, target_var))
                .unwrap_or_else(Type::unknown);
            if !satisfies_bound(db, env, receiver_argument, extension_var) {
                return None;
            }
            bracket_substitution.push(receiver_argument);
        }
    }
    Some(ExtensionApplication {
        receiver_specialization,
        bracket_substitution,
    })
}

/// the member `name` an extension declares in its own body, `None` when it
/// declares none
pub(crate) fn own_member<'db>(
    db: &'db dyn Db,
    extension: StaticClassLiteral<'db>,
    name: &str,
) -> Option<Type<'db>> {
    class_member(db, extension.body_scope(db), name).ignore_possibly_undefined()
}

/// an extension member that applies to one receiver
struct ApplicableMember<'db> {
    member: Type<'db>,
    /// the interface the receiver reached this member through, when it was
    /// reached by conformance rather than by being an instance of the extended
    /// type. that interface is what the member's `self` is written as
    conformed_as: Option<ClassType<'db>>,
}

/// if `extension` applies to `receiver_class` and declares `name`, return the
/// member's type with the receiver's type arguments substituted in.
///
/// An extension of a *protocol* also applies to any class a visible conformance
/// extension conforms to it — that is what makes a protocol extension's members
/// reachable on a conforming type, exactly as they are on the protocol itself
fn applicable_member<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    extension: StaticClassLiteral<'db>,
    receiver_class: ClassType<'db>,
    name: &str,
) -> Option<ApplicableMember<'db>> {
    if let Some(application) = extension_applies(db, env, extension, receiver_class) {
        let member = own_member(db, extension, name)?;
        return Some(ApplicableMember {
            member: application.apply(db, extension, member),
            conformed_as: None,
        });
    }
    let target = extended_class(db, extension)?;
    let conformed = conformance::conformance_for(db, env, file, receiver_class, target)?;
    let application = applied_at(db, env, extension, target, conformed)?;
    let member = own_member(db, extension, name)?;
    Some(ApplicableMember {
        member: application.apply(db, extension, member),
        conformed_as: Some(conformed),
    })
}

/// does the receiver's type argument satisfy a bracket typevar's bound?
fn satisfies_bound<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    argument: Type<'db>,
    extension_var: BoundTypeVarInstance<'db>,
) -> bool {
    match extension_var.typevar(db).bound_or_constraints(db, env) {
        None => true,
        Some(TypeVarBoundOrConstraints::UpperBound(bound)) => {
            argument.is_assignable_to(db, env, bound)
        }
        Some(TypeVarBoundOrConstraints::Constraints(constraints)) => constraints
            .elements(db)
            .iter()
            .any(|constraint| argument.is_assignable_to(db, env, *constraint)),
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
        _ if member.is_instance_of(db, KnownClass::ByStaticProperty) => {
            ExtensionMemberKind::StaticProperty
        }
        _ => ExtensionMemberKind::Method,
    }
}
