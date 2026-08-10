//! basedpython protocol conformance (`extension str(A):`)
//!
//! A conformance extension states that an existing type satisfies an existing
//! protocol, without touching either declaration. It parses to the same
//! [`ClassDef`] an ordinary `extension` does, with the protocols it conforms to
//! in the class's argument list — where a class's bases live, so resolving them
//! is the ordinary base resolution.
//!
//! Everything past that point is this module's, because an extension's members
//! are written against the *extended* type rather than inherited from the
//! interface — the inheritance machinery has nothing to work with:
//!
//! - **the registry**: which conformances a file can see (its own module's plus
//!   those of every module it imports), and which one makes a given class
//!   conform to a given protocol
//! - **the reach**: a conformance makes the protocol's *extension* members
//!   available on the conforming type ([`conformance_for`], consulted by
//!   [`super::extensions`]), and makes the conforming type acceptable where the
//!   protocol is asked for ([`repair_with_conformance`], routed through
//!   [`super::conversions`])
//! - **the witness table**: the requirement-to-function mapping the transpiler
//!   registers at runtime, so a call through a protocol-typed receiver reaches
//!   the conformance's own member rather than an attribute the object does not
//!   have
//! - **the checks**: that the interface is one, that every requirement is
//!   answered, that each answer has the shape the requirement asks for, and that
//!   no two visible conformances claim the same pair
//!
//! Nothing is registered globally and nothing is monkeypatched: a conformance
//! reaches exactly the modules that import the one declaring it, so two
//! dependencies cannot fight over the same pair.
//!
//! [`ClassDef`]: ruff_python_ast::StmtClassDef

use ruff_python_ast as ast;
use ruff_python_ast::name::Name;

use ruff_db::files::File;
use ty_module_resolver::{ModuleName, resolve_module};

use crate::Db;
use crate::types::Type;
use crate::types::class::{ClassLiteral, ClassType, StaticClassLiteral};
use crate::types::context::InferContext;
use crate::types::diagnostic::INVALID_CONFORMANCE;
use crate::types::extensions::{
    applicable_extensions, backing_function_name, extended_class, extension_applies,
    extensions_in_module, own_member,
};

/// the protocols (or abstract classes) a conformance extension declares its
/// target conforms to — the extension's explicit bases, which is exactly where
/// the parser puts the header's argument list
pub(crate) fn declared_conformances<'db>(
    db: &'db dyn Db,
    extension: StaticClassLiteral<'db>,
) -> Vec<ClassType<'db>> {
    if !extension.is_extension(db) {
        return Vec::new();
    }
    extension
        .explicit_bases(db)
        .iter()
        .filter_map(|base| base.to_class_type(db))
        .collect()
}

/// every conformance a file can see, as `(the extension that declares it, the
/// protocol it conforms to)`.
///
/// Visibility is the extension one: a file sees its own module's conformances
/// and those of every module it imports. Cycles for the same reason
/// [`applicable_extensions`] does — resolving a declaration infers module-level
/// code, which asks what conforms — and recovers the same way, from "nothing
/// conforms yet".
#[salsa::tracked(
    returns(deref),
    cycle_initial = |_, _, _| Box::default(),
    heap_size = ruff_memory_usage::heap_size
)]
pub(crate) fn visible_conformances(
    db: &dyn Db,
    file: File,
) -> Box<[(StaticClassLiteral<'_>, ClassType<'_>)]> {
    if !file.source_type(db).is_basedpython() {
        return Box::default();
    }
    let mut conformances = Vec::new();
    for &extension in applicable_extensions(db, file) {
        for protocol in declared_conformances(db, extension) {
            conformances.push((extension, protocol));
        }
    }
    conformances.into_boxed_slice()
}

/// does this module declare any conformance at all?
///
/// A conformance is an *import-time side effect*: the declaring module's
/// `_by_conform(...)` call is what puts it in the registry. A module that
/// declares one therefore cannot be imported lazily, or the conformance simply
/// never exists at runtime
#[salsa::tracked(cycle_initial = |_, _, _| false)]
pub fn declares_conformances(db: &dyn Db, file: File) -> bool {
    if !file.source_type(db).is_basedpython() {
        return false;
    }
    extensions_in_module(db, file)
        .iter()
        .any(|&extension| !declared_conformances(db, extension).is_empty())
}

/// the modules `file` imports that declare a conformance *themselves*, as the
/// spellings `file`'s own import statements use.
///
/// Deliberately one level, not the transitive closure. A conformance is only
/// ever applicable in a file that directly imports the module declaring it —
/// that is [`applicable_extensions`]'s rule, and this has to be its mirror.
/// Following the closure instead would drag a module eager for importing
/// something that imports something that conforms, which unlazifies most of a
/// real import graph for no reach the checker ever grants.
///
/// A module carrying only *ordinary* extensions is untouched: its members are
/// resolved at transpile time and need nothing to have run
pub(crate) fn eagerly_imported_modules(db: &dyn Db, file: File) -> Vec<String> {
    let mut eager = Vec::new();
    for module_name in imported_module_names(db, file) {
        let Some(target) = resolve_module(db, file, &module_name).and_then(|m| m.file(db)) else {
            continue;
        };
        if target != file && *declares_conformances(db, target) {
            eager.push(module_name.to_string());
        }
    }
    eager
}

/// every module named by an `import` or a `from ... import` in `file`
fn imported_module_names(db: &dyn Db, file: File) -> Vec<ModuleName> {
    ty_python_core::semantic_index(db, file)
        .imported_modules()
        .cloned()
        .chain(
            super::conversions::from_imported_modules(db, file)
                .iter()
                .cloned(),
        )
        .collect()
}

/// the protocol a conformance extension in `file` makes `receiver_class`
/// conform to, when that protocol's class literal is `protocol`.
///
/// The answer is the protocol *as the conformance declared it*, so a specialized
/// conformance (`extension MyBox(Container[int]):`) hands back `Container[int]`
/// — which is what a protocol extension's members must be specialized at when
/// they are reached through the conforming type
pub(crate) fn conformance_for<'db>(
    db: &'db dyn Db,
    file: File,
    receiver_class: ClassType<'db>,
    protocol: ClassLiteral<'db>,
) -> Option<ClassType<'db>> {
    for &(extension, declared) in visible_conformances(db, file) {
        if declared.class_literal(db) != protocol {
            continue;
        }
        if extension_applies(db, extension, receiver_class).is_some() {
            return Some(declared);
        }
    }
    None
}

/// would a visible conformance make `source` acceptable where `target` is asked
/// for? the protocol it conforms to, for the diagnostic that reports two routes.
///
/// Unlike a conversion this materializes nothing: the value already answers
/// every requirement, through the witness table its conformance registered at
/// import time. The relation still stays out of the lattice, because a
/// conformance is only visible to the files that import it and the lattice has
/// no file to ask
pub(crate) fn repair_with_conformance<'db>(
    db: &'db dyn Db,
    file: File,
    source: Type<'db>,
    target: Type<'db>,
) -> Option<ClassType<'db>> {
    if !file.source_type(db).is_basedpython() {
        return None;
    }
    let conformances = visible_conformances(db, file);
    if conformances.is_empty() {
        return None;
    }
    // a repair only ever *adds* an assignment that fails without it
    if source.is_assignable_to(db, target) {
        return None;
    }
    // a use-site modifier says nothing about which class the value is an
    // instance of, so `final Widget` finds `Widget`'s conformances
    let source_class = source.erase_restriction(db).nominal_class(db)?;

    for &(extension, protocol) in conformances {
        if extension_applies(db, extension, source_class).is_none() {
            continue;
        }
        if Type::instance(db, protocol).is_assignable_to(db, target) {
            return Some(protocol);
        }
    }
    None
}

/// the members an interface asks a conforming type to supply — a protocol's
/// interface members, which for a protocol is the whole of it
pub(crate) fn interface_requirements<'db>(db: &'db dyn Db, interface: ClassType<'db>) -> Vec<Name> {
    let Some(protocol) = interface.into_protocol_class(db) else {
        return Vec::new();
    };
    protocol
        .interface(db)
        .members(db)
        .map(|member| Name::new(member.name()))
        .collect()
}

/// may `interface` be conformed to at all?
///
/// A protocol, and only a protocol. Three things follow from an abstract class
/// that a conformance cannot honour, and each was a silent miscompile while they
/// were allowed: its *concrete* methods are not requirements, so nothing puts
/// them in the witness table and a call to one on a conforming value is an
/// `AttributeError`; `ABCMeta` alone made a class abstract, so a class with no
/// abstract members at all had zero requirements and conforming to it promised
/// nothing; and every abstract-member access in ordinary ABC code would have to
/// go through the dispatcher, for a mechanism ABCs already have in `register`
/// and inheritance.
///
/// A protocol has none of those: its interface *is* its requirements
pub(crate) fn is_conformable<'db>(db: &'db dyn Db, interface: ClassType<'db>) -> bool {
    interface.class_literal(db).is_protocol(db)
}

/// where a requirement's implementation comes from, once a conformance is
/// resolved
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WitnessEntry {
    /// the requirement being satisfied
    pub(crate) member: String,
    /// the module-level backing function that implements it
    pub(crate) function: String,
    /// the module to import that function from, when it comes from an extension
    /// declared elsewhere
    pub(crate) import_from: Option<String>,
}

/// the witness table one conformance registers at runtime: every requirement an
/// *extension* supplies, whether the conformance's own block or a default on the
/// protocol's extension.
///
/// A requirement the conforming type already answers itself is deliberately
/// absent: the dispatcher falls back to the attribute, so a native member costs
/// no table entry and cannot be shadowed by a stale one
pub(crate) fn witness_table<'db>(
    db: &'db dyn Db,
    from_file: File,
    conformance: StaticClassLiteral<'db>,
    protocol: ClassType<'db>,
) -> Vec<WitnessEntry> {
    let mut entries = Vec::new();
    for requirement in interface_requirements(db, protocol) {
        let name = requirement.as_str();
        let supplier = if own_member(db, conformance, name).is_some() {
            Some(conformance)
        } else {
            protocol_extension_supplying(db, from_file, protocol.class_literal(db), name)
        };
        let Some(supplier) = supplier else {
            continue;
        };
        let import_from = if supplier.file(db) == from_file {
            None
        } else {
            match super::conversions::imported_module_spelling(db, from_file, supplier.file(db)) {
                Some(module) => Some(module),
                // the extension is not reachable from here; the checker has
                // already declined to make the conformance applicable
                None => continue,
            }
        };
        entries.push(WitnessEntry {
            member: name.to_owned(),
            function: backing_function_name(db, supplier, name),
            import_from,
        });
    }
    entries
}

/// the visible `extension <protocol>:` block that supplies a default body for
/// `name`, if any
fn protocol_extension_supplying<'db>(
    db: &'db dyn Db,
    file: File,
    protocol: ClassLiteral<'db>,
    name: &str,
) -> Option<StaticClassLiteral<'db>> {
    applicable_extensions(db, file)
        .iter()
        .copied()
        .find(|&extension| {
            extended_class(db, extension) == Some(protocol)
                && own_member(db, extension, name).is_some()
        })
}

/// does a receiver typed as `interface` need dynamic dispatch for `name`?
///
/// Only a *requirement* does. An inherent member of a protocol extension is
/// resolved statically to its backing function like any other extension member,
/// and a member the interface does not declare is not the interface's business.
///
/// This deliberately does **not** ask whether any conformance is visible here.
/// A conformance is written in the module that *imports* the interface, so the
/// module declaring the protocol-typed function can never see one — gating on
/// local visibility emitted a plain attribute access in exactly the case the
/// feature exists for. The dispatcher falls back to `getattr` when nothing is
/// registered, so dispatching unconditionally costs one lookup in a registry
/// that is empty for programs which use no conformances
pub(crate) fn requires_witness_dispatch<'db>(
    db: &'db dyn Db,
    interface: ClassType<'db>,
    name: &str,
) -> bool {
    // a project with no conformance anywhere can never need the table, and
    // rewriting every protocol member access in it would be pure noise — worse,
    // it would name the protocol at runtime, which a `TYPE_CHECKING`-only import
    // cannot survive. this is the *only* gate: a per-file one is unsound, since
    // a conformance is written downstream of the interface it conforms to
    if !db.project_declares_conformances() {
        return false;
    }
    interface_requirements(db, interface)
        .iter()
        .any(|requirement| requirement.as_str() == name)
}

/// how one conformance registers itself at runtime, for the transpiler
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceRegistration {
    /// the interface, spelled so that it resolves at the declaration site
    pub interface: String,
    /// the import that spelling needs, when the interface is declared elsewhere
    pub import: Option<super::conversions::ConversionImport>,
    /// `(requirement, the backing function that answers it)`, for every
    /// requirement an extension supplies
    pub entries: Vec<(String, String)>,
    /// the `from <module> import <function>` lines those functions need
    pub imports: Vec<String>,
}

/// how one attribute access dispatches through a witness table.
///
/// A requirement read off an interface-typed receiver cannot be a plain
/// attribute: the value may be a conforming type that carries no such member of
/// its own, and only the table its conformance registered knows what to call
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WitnessDispatch {
    /// the interface, spelled so that it resolves at the access site
    pub interface: String,
    /// the import that spelling needs, when the interface is declared elsewhere
    pub import: Option<super::conversions::ConversionImport>,
    pub member: String,
    /// how the receiver reaches the witness
    pub kind: WitnessKind,
}

/// what the dispatcher does with the receiver it is handed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WitnessKind {
    /// a method: the witness is fetched and the call parentheses already
    /// following the access apply it
    Method,
    /// a `class def`: the witness takes the class, so an instance receiver is
    /// widened the way the static lowering widens one
    ClassMethod,
    /// a data member: the witness is read, not called
    Property,
}

/// how `x is <interface>` is answered at runtime once conformances are in play
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceTest {
    /// the requirement names, for a protocol target: with no registered
    /// conformance for the value's class, answering the test means checking that
    /// the value carries them. `None` for an abstract class, where `isinstance`
    /// is already the whole answer
    pub members: Option<Vec<String>>,
}

/// declaration-site validation for a conformance extension, run from the
/// post-inference static-class checks alongside the ordinary extension checks
pub(crate) fn validate_conformance_declaration<'db>(
    context: &InferContext<'db, '_>,
    extension: StaticClassLiteral<'db>,
    class_node: &ast::StmtClassDef,
) {
    let db = context.db();
    let declared = declared_conformances(db, extension);
    if declared.is_empty() {
        return;
    }
    let Some(target) = extended_class(db, extension) else {
        // the extended name does not resolve; `validate_extension_declaration`
        // has already reported that, and everything here would repeat it
        return;
    };

    // `explicit_bases` and the header's own base list are positionally aligned,
    // so each interface is anchored on the base that produced it. filtering
    // first and indexing after would slide every later interface onto the wrong
    // span as soon as one base did not resolve to a class
    for (base_ty, base_node) in extension
        .explicit_bases(db)
        .iter()
        .zip(class_node.bases().iter())
    {
        let node: &dyn ruff_text_size::Ranged = base_node;
        let Some(interface) = base_ty.to_class_type(db) else {
            if let Some(builder) = context.report_lint(&INVALID_CONFORMANCE, node.range()) {
                let mut diagnostic =
                    builder.into_diagnostic("a conformance list names interfaces".to_string());
                diagnostic.info(format_args!("`{}` is not a class", base_ty.display(db)));
            }
            continue;
        };
        let interface = &interface;
        let interface_instance = Type::instance(db, *interface);
        if !is_conformable(db, *interface) {
            if let Some(builder) = context.report_lint(&INVALID_CONFORMANCE, node.range()) {
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "`{}` is not a protocol",
                    interface_instance.display(db),
                ));
                diagnostic.info(
                    "a conformance names a protocol; an abstract class carries concrete members a \
                     conformance could never answer, and already has inheritance and `register`",
                );
            }
            continue;
        }

        // two conformances of one pair would register two witness tables against
        // the same key, and which one survives would depend on import order
        if let Some(other) = conflicting_conformance(db, extension, target, *interface)
            && let Some(builder) = context.report_lint(&INVALID_CONFORMANCE, node.range())
        {
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "`{}` is already conformed to `{}` here",
                target.name(db),
                interface_instance.display(db),
            ));
            diagnostic.info(format_args!(
                "the other conformance is declared in `{}`",
                other.file(db).path(db),
            ));
            diagnostic.help(
                "Constrain one of them, or drop the import that brings the second into scope",
            );
        }

        // the registration is an ordinary statement naming two ordinary names, so
        // both have to be bound by the time it runs. the backing functions are
        // hoisted above it, but a class declared *below* it is not
        for (label, declared) in [
            ("the protocol it conforms to", interface.class_literal(db)),
            ("the type it extends", target),
        ] {
            if let ClassLiteral::Static(declared) = declared
                && declared.file(db) == extension.file(db)
                && declared.header_range(db).start() > class_node.range.start()
                && let Some(builder) = context.report_lint(&INVALID_CONFORMANCE, node.range())
            {
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "`{}` is declared after this conformance",
                    declared.name(db),
                ));
                diagnostic.info(format_args!(
                    "a conformance registers itself where it is written, so {label} has to \
                     already exist"
                ));
                diagnostic.help("Move the conformance below the declaration");
            }
        }

        // a bracket bound narrows *which specializations* conform, and nothing in
        // the runtime table can carry that: it is keyed by class, so a bounded
        // conformance would register for every specialization and hand
        // `list[str]` the `list[int]` witness
        if class_node.type_params.is_some()
            && let Some(builder) = context.report_lint(&INVALID_CONFORMANCE, node.range())
        {
            let mut diagnostic =
                builder.into_diagnostic("a conformance may not carry a bracket bound".to_string());
            diagnostic.info(
                "conformance is registered per class, so a bound could not be checked where a \
                 value is dispatched on",
            );
            diagnostic.help(
                "Declare the members in a bounded `extension`, and conform the type \
                 unconditionally",
            );
        }

        // an ordinary `extension` may supply an operator's dunder, because that
        // lowering rewrites the operator at the use site from the *concrete*
        // operand type. a requirement is by definition reached through the
        // interface, where the concrete type is exactly what is not known — and
        // python resolves a dunder on the type, so no dispatcher can be
        // interposed either. a type that already has the dunder needs no witness
        // for it and conforms fine
        for requirement in interface_requirements(db, *interface) {
            let name = requirement.as_str();
            let supplied_here = own_member(db, extension, name).is_some()
                || protocol_extension_supplying(
                    db,
                    extension.file(db),
                    interface.class_literal(db),
                    name,
                )
                .is_some();
            if supplied_here
                && name.starts_with("__")
                && name.ends_with("__")
                && let Some(builder) = context.report_lint(&INVALID_CONFORMANCE, node.range())
            {
                let mut diagnostic =
                    builder.into_diagnostic(format_args!("a conformance cannot supply `{name}`"));
                diagnostic.info(
                    "an operator is rewritten from the concrete operand type, and python resolves \
                     a dunder on the type — neither can reach a value typed as the interface",
                );
                diagnostic.help(format_args!(
                    "declare `{name}` on `{}` itself, or in an `extension` that carries no \
                     conformance",
                    target.name(db),
                ));
            }
        }

        // a member that answers a requirement has to be usable as one: every call
        // through the interface goes to it, and the extension's members are
        // written against the *extended* type rather than inherited from the
        // interface, so nothing else would catch a mismatch.
        //
        // this covers a requirement the *target* already answers too. leaving
        // that case out was a hole with teeth: `target_declares` satisfied the
        // missing-member check below while the shape went unlooked-at, so a
        // `def show(self) -> int` silently answered a `-> str` requirement
        for requirement in interface_requirements(db, *interface) {
            let name = requirement.as_str();
            let supplied = bound_own_member(db, extension, name)
                .or_else(|| bound_target_member(db, target, name));
            let (Some(supplied), Some(expected)) = (
                supplied,
                interface_instance
                    .member(db, name)
                    .place
                    .ignore_possibly_undefined()
                    .map(|expected| shed_receiver(db, expected)),
            ) else {
                continue;
            };
            if !supplied.is_assignable_to(db, expected)
                && let Some(builder) = context.report_lint(&INVALID_CONFORMANCE, node.range())
            {
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "`{name}` does not match the member `{}` declares",
                    interface_instance.display(db),
                ));
                diagnostic.info(format_args!(
                    "expected `{}`, found `{}`",
                    expected.display(db),
                    supplied.display(db),
                ));
            }
        }

        // every requirement has to be answered by something: the block itself, a
        // default on the protocol's own extension, or a member the type already
        // has. an unanswered one is an `AttributeError` the moment anything
        // dispatches through the conformance
        let missing: Vec<String> = interface_requirements(db, *interface)
            .into_iter()
            .filter(|requirement| {
                let name = requirement.as_str();
                own_member(db, extension, name).is_none()
                    && protocol_extension_supplying(
                        db,
                        extension.file(db),
                        interface.class_literal(db),
                        name,
                    )
                    .is_none()
                    && !target_declares(db, target, name)
            })
            .map(|requirement| format!("`{requirement}`"))
            .collect();
        if !missing.is_empty()
            && let Some(builder) = context.report_lint(&INVALID_CONFORMANCE, node.range())
        {
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "`{}` does not answer every member of `{}`",
                target.name(db),
                interface_instance.display(db),
            ));
            diagnostic.info(format_args!("missing: {}", missing.join(", ")));
            diagnostic.help(
                "Supply them in this block, or add a default on an `extension` of the interface",
            );
        }
    }
}

/// the member `name` an extension declares in its own body, bound against the
/// extended type — the shape a caller reaching it through the interface gets
fn bound_own_member<'db>(
    db: &'db dyn Db,
    extension: StaticClassLiteral<'db>,
    name: &str,
) -> Option<Type<'db>> {
    let member = own_member(db, extension, name)?;
    let receiver = Type::instance(db, super::extensions::body_view_class(db, extension)?);
    let bound = member
        .try_call_dunder_get(db, Some(receiver), receiver.to_meta_type(db))
        .map_or(member, |(bound, _)| bound);
    Some(shed_receiver(db, bound))
}

/// a bound method as the plain callable it is once bound.
///
/// The comparison this feeds is about *shape*: a conformance's member and the
/// requirement it answers are bound to different classes by construction, so
/// comparing the bound methods themselves would reject every conformance
fn shed_receiver<'db>(db: &'db dyn Db, ty: Type<'db>) -> Type<'db> {
    match ty {
        Type::BoundMethod(method) => Type::Callable(method.into_callable_type(db)),
        other => other,
    }
}

/// the member `name` the conforming type answers itself, bound against it
fn bound_target_member<'db>(
    db: &'db dyn Db,
    target: ClassLiteral<'db>,
    name: &str,
) -> Option<Type<'db>> {
    let receiver = Type::instance(db, target.unknown_specialization(db));
    let member = receiver
        .member(db, name)
        .place
        .ignore_possibly_undefined()?;
    Some(shed_receiver(db, member))
}

/// another visible conformance of the same pair as `extension`'s, when this
/// declaration is the one that should report it.
///
/// A duplicate in the same module is reported at the *second* of the two, so one
/// conflict is one diagnostic. A conflict with an imported module is always
/// reported here: that module cannot see this one, so nothing over there would
fn conflicting_conformance<'db>(
    db: &'db dyn Db,
    extension: StaticClassLiteral<'db>,
    target: ClassLiteral<'db>,
    interface: ClassType<'db>,
) -> Option<StaticClassLiteral<'db>> {
    let mut seen_self = false;
    for &(candidate, declared) in visible_conformances(db, extension.file(db)) {
        if candidate == extension {
            seen_self = true;
            continue;
        }
        if declared.class_literal(db) != interface.class_literal(db)
            || extended_class(db, candidate) != Some(target)
        {
            continue;
        }
        if candidate.file(db) != extension.file(db) || !seen_self {
            return Some(candidate);
        }
    }
    None
}

/// does the conforming type already answer `name` itself, without the
/// conformance having to supply it?
fn target_declares<'db>(db: &'db dyn Db, target: ClassLiteral<'db>, name: &str) -> bool {
    // `object`'s members count: every value really does answer `__str__` and
    // friends at runtime, and excluding them reported a `Stringy` protocol as
    // unanswered by a class that satisfies it perfectly well
    !Type::instance(db, target.unknown_specialization(db))
        .member(db, name)
        .place
        .is_undefined()
}
