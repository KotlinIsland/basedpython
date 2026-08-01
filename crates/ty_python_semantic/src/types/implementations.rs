//! basedpython implementation declarations (`implementation A for B:`)
//!
//! an implementation is a retroactive statement that an existing type satisfies
//! an existing interface, without touching either declaration. the parser lowers
//! the block to a [`ClassDef`] carrying an [`ImplementationHeader`]; that class
//! *is* the witness type — it derives the interface (see
//! `StmtClassDef::base_exprs`), so subtyping, the MRO, `override` checking and
//! abstract-member enforcement all come from the ordinary class machinery.
//!
//! what this module adds is the three things that do not:
//!
//! - **the registry**: which implementations are applicable in a file (its own
//!   plus those of every module it imports with a plain `import mod`), and
//!   whether two of them collide on the same interface-and-type pair
//! - **the member fallback**: a witness reaches the *implemented* type's members
//!   as well as the interface's, because that is what an implementation body
//!   needs (`self.a` where `a` is a field of `B`). at runtime this is
//!   `__getattr__` forwarding to `__implemented__`
//! - **the repair**: `B` is not a subtype of `A` anywhere in the lattice.
//!   [`repair_with_implementation`] answers "would a witness make this
//!   assignment work?" at the positions where the transpiler can materialize
//!   one, which is what keeps `list[B]` from being a `Sequence[A]`
//!
//! [`ClassDef`]: ruff_python_ast::StmtClassDef
//! [`ImplementationHeader`]: ruff_python_ast::ImplementationHeader

use ruff_db::files::File;
use ruff_python_ast as ast;
use ruff_text_size::Ranged;
use ty_module_resolver::{ModuleName, resolve_module};
use ty_python_core::semantic_index;

use crate::Db;
use crate::place::{PlaceAndQualifiers, builtins_symbol, global_symbol};
use crate::types::class::{ClassLiteral, ClassType, StaticClassLiteral};
use crate::types::context::InferContext;
use crate::types::diagnostic::{AMBIGUOUS_CONVERSION, INVALID_IMPLEMENTATION};
use crate::types::{KnownClass, Type};

/// the member every witness carries: the object it wraps. an implementation body
/// reaches the implemented value through it when the implemented type's own
/// member is shadowed, or when it needs to hand the real object to something
/// that wants a `B`
pub(crate) const IMPLEMENTED_ATTRIBUTE: &str = "__implemented__";

/// all module-level implementation declarations in a module, in source order.
///
/// Read off the module's own statements rather than by typing every global symbol:
/// a named implementation binds an ordinary name, so a symbol-table scan would
/// have to infer the type of *every* global to find them — making this query
/// depend on all of them, and re-running it whenever any one changes. The AST walk
/// touches only the classes that carry a header, and gives source order for free.
#[salsa::tracked(returns(deref), heap_size = ruff_memory_usage::heap_size)]
pub(crate) fn implementations_in_module(db: &dyn Db, file: File) -> Box<[StaticClassLiteral<'_>]> {
    // only basedpython files declare implementations. a `.py` file containing an
    // `implementation` block already has a parse error; don't serve it
    if !file.source_type(db).is_basedpython() {
        return Box::default();
    }
    let module = ruff_db::parsed::parsed_module(db, file).load(db);
    let index = semantic_index(db, file);
    let mut implementations: Vec<StaticClassLiteral<'_>> = Vec::new();
    for stmt in &module.syntax().body {
        let ast::Stmt::ClassDef(class) = stmt else {
            continue;
        };
        if !class.is_implementation() {
            continue;
        }
        let definition = index.expect_single_definition(class);
        let Some(literal) = crate::types::infer::original_class_type(db, definition)
            .and_then(super::class::ClassLiteral::as_static)
        else {
            continue;
        };
        // a second implementation of a pair already declared in this module is
        // invalid (reported at its own declaration); ignoring it here keeps every
        // conversion site unambiguous and deterministic rather than making it
        // depend on which of the two came first
        if implementations.iter().any(|earlier| {
            implemented_class(db, *earlier) == implemented_class(db, literal)
                && implemented_interface(db, *earlier) == implemented_interface(db, literal)
        }) {
            continue;
        }
        implementations.push(literal);
    }
    implementations.into_boxed_slice()
}

/// the implementations applicable in `file`: its own, then those of every module
/// it imports — in either form (in that order).
///
/// **Both** `import mod` and `from mod import X` count. Importing the interface
/// and the implemented type by name is the natural way to write this, and it is
/// what establishes the dependency on the adapting module; requiring a separate
/// `import mod` whose symbols are never used would leave an import that reads as
/// removable to anyone tidying the file, silently withdrawing conformance.
///
/// nothing is registered globally, so two dependencies that implement the same
/// pair cannot collide unless one file imports both — which is reported at the
/// conversion site rather than silently resolved by ordering
#[salsa::tracked(returns(deref), heap_size = ruff_memory_usage::heap_size)]
pub(crate) fn applicable_implementations(db: &dyn Db, file: File) -> Box<[StaticClassLiteral<'_>]> {
    if !file.source_type(db).is_basedpython() {
        return Box::default();
    }
    let mut implementations: Vec<StaticClassLiteral<'_>> =
        implementations_in_module(db, file).to_vec();
    // `imported_modules` deliberately records only `import mod` (see its docs), so
    // the `from mod import X` forms are collected from the file's own statements
    let imported = semantic_index(db, file)
        .imported_modules()
        .chain(from_imported_modules(db, file));
    for module_name in imported {
        let Some(module_file) =
            resolve_module(db, file, module_name).and_then(|module| module.file(db))
        else {
            continue;
        };
        if module_file == file {
            continue;
        }
        for &implementation in implementations_in_module(db, module_file) {
            if !implementations.contains(&implementation) {
                implementations.push(implementation);
            }
        }
    }
    implementations.into_boxed_slice()
}

/// the search state for `imported_module_spelling`: the first import statement
/// resolving to `target` wins
struct ImportSpelling<'a> {
    db: &'a dyn Db,
    from_file: File,
    target: File,
    found: Option<String>,
}

impl ImportSpelling<'_> {
    fn resolves(&self, name: &ModuleName) -> bool {
        resolve_module(self.db, self.from_file, name).and_then(|module| module.file(self.db))
            == Some(self.target)
    }
}

impl<'ast> ast::visitor::Visitor<'ast> for ImportSpelling<'_> {
    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        if self.found.is_some() {
            return;
        }
        match stmt {
            ast::Stmt::Import(import) => {
                for alias in &import.names {
                    if let Some(name) = ModuleName::new(&alias.name)
                        && self.resolves(&name)
                    {
                        self.found = Some(alias.name.to_string());
                        return;
                    }
                }
            }
            ast::Stmt::ImportFrom(import) => {
                if let Ok(name) = ModuleName::from_import_statement(self.db, self.from_file, import)
                    && self.resolves(&name)
                {
                    // keep the leading dots: a relative import is how this file
                    // addresses the module, and the absolute name may not resolve
                    let mut spelling = ".".repeat(import.level as usize);
                    if let Some(module) = &import.module {
                        spelling.push_str(module);
                    }
                    self.found = Some(spelling);
                    return;
                }
            }
            _ => {}
        }
        ast::visitor::walk_stmt(self, stmt);
    }
}

/// how `from_file` spells the module that `target` is, in its own imports.
///
/// A synthesized import has to address the module the way the importing file
/// already does. ty's absolute module name is not usable for that: a file under a
/// directory that is not an importable package still resolves for the checker
/// (`target/mod.by` → `target.mod`), while the interpreter running the output only
/// sees `mod` — and a relative import has no absolute spelling at all. Since a
/// cross-module implementation is only applicable when the file imports the module
/// that declares it, a spelling always exists.
pub(crate) fn imported_module_spelling(
    db: &dyn Db,
    from_file: File,
    target: File,
) -> Option<String> {
    let module = ruff_db::parsed::parsed_module(db, from_file).load(db);
    let mut spelling = ImportSpelling {
        db,
        from_file,
        target,
        found: None,
    };
    for stmt in &module.syntax().body {
        ast::visitor::Visitor::visit_stmt(&mut spelling, stmt);
        if spelling.found.is_some() {
            break;
        }
    }
    spelling.found
}

/// every module named by a `from <module> import ...` statement anywhere in
/// `file`, relative imports resolved
#[salsa::tracked(returns(deref), heap_size = ruff_memory_usage::heap_size)]
fn from_imported_modules(db: &dyn Db, file: File) -> Box<[ModuleName]> {
    struct Collector<'a> {
        db: &'a dyn Db,
        file: File,
        modules: Vec<ModuleName>,
    }
    impl<'ast> ast::visitor::Visitor<'ast> for Collector<'_> {
        fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
            if let ast::Stmt::ImportFrom(import) = stmt
                && let Ok(name) = ModuleName::from_import_statement(self.db, self.file, import)
                && !self.modules.contains(&name)
            {
                self.modules.push(name);
            }
            ast::visitor::walk_stmt(self, stmt);
        }
    }

    let module = ruff_db::parsed::parsed_module(db, file).load(db);
    let mut collector = Collector {
        db,
        file,
        modules: Vec::new(),
    };
    for stmt in &module.syntax().body {
        // `visit_stmt`, not `walk_stmt`: the latter descends into the statement's
        // children and would skip the statement itself
        ast::visitor::Visitor::visit_stmt(&mut collector, stmt);
    }
    collector.modules.into_boxed_slice()
}

/// the class an implementation implements *for*: the header's name resolved in
/// the declaring module's globals, else builtins. `None` when the name does not
/// resolve to a class (reported at the declaration)
#[salsa::tracked(returns(copy))]
pub(crate) fn implemented_class<'db>(
    db: &'db dyn Db,
    implementation: StaticClassLiteral<'db>,
) -> Option<ClassLiteral<'db>> {
    if !implementation.is_implementation(db) {
        return None;
    }
    let name = implementation.implemented_type_name(db).as_ref()?;
    let file = implementation.file(db);
    let resolved = global_symbol(db, file, name)
        .place
        .ignore_possibly_undefined()
        .or_else(|| builtins_symbol(db, name).place.ignore_possibly_undefined())?;
    let literal = resolved.as_class_literal()?;
    // an implementation *of* or *for* a witness class is meaningless; the
    // mangled binding makes the anonymous case unreachable, but a named one is
    // an ordinary symbol
    if let ClassLiteral::Static(static_literal) = literal
        && (static_literal.is_implementation(db) || static_literal.is_extension(db))
    {
        return None;
    }
    Some(literal)
}

/// the implemented class as the witness's body sees it: specialized at the
/// bracket-declared (bounded) typevar where one is spelled, else at the
/// implemented class's own typevar. this is what a member access on a witness
/// falls back to
pub(crate) fn implemented_view_class<'db>(
    db: &'db dyn Db,
    implementation: StaticClassLiteral<'db>,
) -> Option<ClassType<'db>> {
    let target = implemented_class(db, implementation)?;
    let Some(target_context) = target.generic_context(db) else {
        return Some(ClassType::NonGeneric(target));
    };
    let implementation_context = implementation.generic_context(db);
    let types: Vec<Type<'db>> = target_context
        .variables(db)
        .map(|target_var| {
            let spelled = implementation_context.and_then(|context| {
                context.binds_named_typevar(db, target_var.typevar(db).name(db))
            });
            Type::TypeVar(spelled.unwrap_or(target_var))
        })
        .collect();
    Some(target.apply_specialization(db, |context| context.specialize(db, types)))
}

/// the interface an implementation declares conformance to. it is the witness
/// class's first explicit base, because the parser puts the header's interface
/// there (see `StmtClassDef::base_exprs`)
pub(crate) fn implemented_interface<'db>(
    db: &'db dyn Db,
    implementation: StaticClassLiteral<'db>,
) -> Option<ClassType<'db>> {
    if !implementation.is_implementation(db) {
        return None;
    }
    implementation
        .explicit_bases(db)
        .first()
        .copied()?
        .to_class_type(db)
}

/// forward a member lookup that found nothing on a witness to the implemented
/// object — `self.a` inside an implementation body, and any member of `B` on a
/// witness value. `__implemented__` names the wrapped object itself.
///
/// this mirrors the `__getattr__` forwarding the emitted witness class does at
/// runtime, and it is a *member* fallback only: it does not make a witness a
/// subtype of the implemented type, which is what stops a witness from flowing
/// into a position that wants the real object
pub(crate) fn witness_member_forward<'db>(
    db: &'db dyn Db,
    receiver: Type<'db>,
    name: &str,
    result: PlaceAndQualifiers<'db>,
) -> PlaceAndQualifiers<'db> {
    if !result.place.is_undefined() {
        return result;
    }
    let Some(witness) = receiver
        .nominal_class(db)
        .map(|class| class.class_literal(db))
        .and_then(|literal| match literal {
            ClassLiteral::Static(static_literal) if static_literal.is_implementation(db) => {
                Some(static_literal)
            }
            _ => None,
        })
    else {
        return result;
    };
    let Some(implemented) = implemented_view_class(db, witness) else {
        return result;
    };
    let implemented_instance = Type::instance(db, implemented);
    if name == IMPLEMENTED_ATTRIBUTE {
        return crate::place::Place::bound(implemented_instance).into();
    }
    let forwarded = implemented_instance.member(db, name);
    if forwarded.place.is_undefined() {
        return result;
    }
    forwarded
}

/// a conversion the checker found for an assignment that would otherwise fail
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImplementationRepair<'db> {
    /// the witness class to construct
    pub(crate) witness: StaticClassLiteral<'db>,
    /// another applicable implementation of the same pair — an ambiguity the
    /// checker reports at the conversion site
    pub(crate) ambiguous_with: Option<StaticClassLiteral<'db>>,
}

/// would an in-scope implementation make `source` assignable to `target`?
///
/// this is the whole of implementation conformance in the type system: `B` is
/// never a subtype of `A`, so only the positions that ask this question — the
/// ones where the transpiler can wrap the expression — accept a `B` for an `A`.
/// nothing nested inside a generic can ask, which is why `list[B]` is not a
/// `Sequence[A]`
pub(crate) fn repair_with_implementation<'db>(
    db: &'db dyn Db,
    file: File,
    source: Type<'db>,
    target: Type<'db>,
) -> Option<ImplementationRepair<'db>> {
    if !file.source_type(db).is_basedpython() {
        return None;
    }
    // a repair only ever *adds* an assignment that fails without it
    if source.is_assignable_to(db, target) {
        return None;
    }
    // a use-site modifier restricts which values reach here, not which class they
    // are instances of, so `final B` finds `B`'s implementations
    let source_class = source
        .erase_restriction(db)
        .nominal_class(db)?
        .class_literal(db);

    let mut repair: Option<ImplementationRepair<'db>> = None;
    for &implementation in applicable_implementations(db, file) {
        if implemented_class(db, implementation) != Some(source_class) {
            continue;
        }
        let witness = Type::instance(db, implementation.identity_specialization(db));
        if !witness.is_assignable_to(db, target) {
            continue;
        }
        match &mut repair {
            None => {
                repair = Some(ImplementationRepair {
                    witness: implementation,
                    ambiguous_with: None,
                });
            }
            Some(repair) => {
                if repair.ambiguous_with.is_none() {
                    repair.ambiguous_with = Some(implementation);
                }
            }
        }
    }
    repair
}

/// report a conversion site served by more than one applicable implementation.
/// the site is otherwise accepted — one of the witnesses would work — but which
/// one runs must not depend on ordering
pub(crate) fn report_ambiguous_implementation<'db>(
    context: &InferContext<'db, '_>,
    node: impl Ranged,
    repair: &ImplementationRepair<'db>,
) {
    let db = context.db();
    let Some(other) = repair.ambiguous_with else {
        return;
    };
    let Some(builder) = context.report_lint(&AMBIGUOUS_CONVERSION, node.range()) else {
        return;
    };
    let mut diagnostic = builder.into_diagnostic(format_args!(
        "More than one applicable implementation converts `{}` here",
        repair
            .witness
            .implemented_type_name(db)
            .as_ref()
            .map_or_else(|| "the value".to_string(), ToString::to_string),
    ));
    diagnostic.info(format_args!(
        "`{}` and `{}` both apply",
        repair.witness.name(db),
        other.name(db),
    ));
    diagnostic.help("Constrain one of them, or drop the import that brings the second into scope");
}

/// declaration-site validation, run from the post-inference static-class checks
pub(crate) fn validate_implementation_declaration<'db>(
    context: &InferContext<'db, '_>,
    implementation: StaticClassLiteral<'db>,
    class_node: &ast::StmtClassDef,
) {
    let db = context.db();
    let Some(header) = class_node.implementation.as_deref() else {
        return;
    };

    // the registry only enumerates module-level declarations, and the lowering only
    // rewrites top-level statements, so a nested block would type-check as a class
    // and then leak its surface syntax into the output
    if !declared_at_module_level(db, implementation)
        && let Some(builder) = context.report_lint(&INVALID_IMPLEMENTATION, &class_node.name)
    {
        let mut diagnostic = builder
            .into_diagnostic("an `implementation` must be declared at module level".to_string());
        diagnostic.info("only module-level implementations are applicable, in this module or in one that imports it");
        return;
    }

    let Some(implemented) = implemented_class(db, implementation) else {
        if let Some(builder) = context.report_lint(&INVALID_IMPLEMENTATION, &class_node.name) {
            builder.into_diagnostic(format_args!(
                "`{}` is not a class; an implementation must name an existing class to \
                implement the interface for",
                class_node.name,
            ));
        }
        return;
    };

    // the interface must be a declared interface: an abstract class or a
    // protocol. a concrete class would mean promising its fields, and the witness
    // has nowhere to hold them
    let Some(interface) = implemented_interface(db, implementation) else {
        return;
    };
    let interface_instance = Type::instance(db, interface);
    if !interface_is_implementable(db, interface) {
        if let Some(builder) = context.report_lint(&INVALID_IMPLEMENTATION, &header.interface) {
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "`{}` is not an abstract class or a protocol",
                interface_instance.display(db),
            ));
            diagnostic
                .info("an implementation's interface must declare behaviour without stored state");
        }
        return;
    }

    // an implementation of an interface the type already satisfies would never be
    // converted — no conversion site would ever fire, so the block is dead code
    let implemented_instance = Type::instance(db, implemented.default_specialization(db));
    if implemented_instance.is_assignable_to(db, interface_instance)
        && let Some(builder) = context.report_lint(&INVALID_IMPLEMENTATION, &header.interface)
    {
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "`{}` already satisfies `{}`",
            implemented.name(db),
            interface_instance.display(db),
        ));
        diagnostic.info("no conversion would ever use this implementation");
    }

    // a second implementation of the same pair in one module is an ambiguity at
    // every conversion site; report it where it is introduced instead
    if let Some(previous) = implementations_in_module(db, implementation.file(db))
        .iter()
        .take_while(|candidate| **candidate != implementation)
        .find(|candidate| {
            implemented_class(db, **candidate) == Some(implemented)
                && implemented_interface(db, **candidate) == Some(interface)
        })
        && let Some(builder) = context.report_lint(&INVALID_IMPLEMENTATION, &class_node.name)
    {
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "`{}` is already implemented for `{}` in this module",
            interface_instance.display(db),
            implemented.name(db),
        ));
        diagnostic.info(format_args!("first implemented by `{}`", previous.name(db)));
    }

    // every abstract member without a default body must be supplied. the witness
    // derives the interface, so ty already knows which are missing — but an
    // anonymous implementation is never instantiated in source, so there is no
    // call site for the ordinary abstract-instantiation error to land on
    // the names the block supplies. an implementation may provide an interface's
    // valueless declaration as a class-level constant, so assignments count too
    let supplied: Vec<&str> = class_node
        .body
        .iter()
        .filter_map(|stmt| match stmt {
            ast::Stmt::FunctionDef(function) => Some(function.name.as_str()),
            ast::Stmt::AnnAssign(annotated) => Some(annotated.target.as_name_expr()?.id.as_str()),
            ast::Stmt::Assign(assign) => match assign.targets.as_slice() {
                [ast::Expr::Name(name)] => Some(name.id.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect();

    // a witness never runs the interface's constructor — it holds the implemented
    // object and nothing else — so an interface that needs one cannot be
    // implemented. its state would silently never exist
    if interface_declares(db, interface, "__init__")
        && let Some(builder) = context.report_lint(&INVALID_IMPLEMENTATION, &header.interface)
    {
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "`{}` defines `__init__`, so it cannot be implemented",
            interface_instance.display(db),
        ));
        diagnostic.info(
            "a witness holds the implemented object and never runs the interface's             constructor, so state assigned there would never exist",
        );
    }

    // an annotation with no value has no runtime existence on the interface, so
    // the block must supply it or reading it through the witness fails
    let interface_declarations: &[ast::name::Name] = match interface.class_literal(db) {
        ClassLiteral::Static(static_interface) => static_interface.valueless_declarations(db),
        _ => &[],
    };
    for declared in interface_declarations {
        if !supplied.contains(&declared.as_str())
            && let Some(builder) = context.report_lint(&INVALID_IMPLEMENTATION, &class_node.name)
        {
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "`{}` declares `{declared}` without a value, so this implementation must supply it",
                interface_instance.display(db),
            ));
            diagnostic.info(
                "only the interface's constructor would otherwise assign it, and a witness never runs it",
            );
        }
    }

    //
    // only members that supply no behaviour of their own: an `abstract def` with a
    // real body is a *default* the witness inherits, exactly as it would for an
    // ordinary subclass, so leaving it out is the point rather than an omission
    let unimplemented: Vec<_> = implementation
        .identity_specialization(db)
        .abstract_methods(db)
        .keys()
        .filter(|name| {
            interface_instance
                .member(db, name)
                .place
                .ignore_possibly_undefined()
                .and_then(|member| match member {
                    Type::FunctionLiteral(function) => Some(function),
                    Type::BoundMethod(method) => Some(method.function(db)),
                    _ => None,
                })
                .is_none_or(|function| function.has_trivial_body(db))
        })
        .cloned()
        .collect();
    if !unimplemented.is_empty()
        && let Some(builder) = context.report_lint(&INVALID_IMPLEMENTATION, &class_node.name)
    {
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "`{}` does not implement every abstract member of `{}`",
            implemented.name(db),
            interface_instance.display(db),
        ));
        diagnostic.info(format_args!(
            "missing: {}",
            unimplemented
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }

    // only members that correspond to something on the interface: an
    // implementation promises conformance, an extension adds inherent members
    for stmt in &class_node.body {
        let member_name = match stmt {
            ast::Stmt::FunctionDef(function) => Some(&function.name),
            ast::Stmt::ClassDef(nested) => {
                if let Some(builder) = context.report_lint(&INVALID_IMPLEMENTATION, nested) {
                    builder.into_diagnostic(
                        "an implementation supplies interface members; nested classes are not \
                        allowed",
                    );
                }
                continue;
            }
            _ => continue,
        };
        let Some(member_name) = member_name else {
            continue;
        };
        if interface_instance
            .member(db, member_name.as_str())
            .place
            .is_undefined()
            && let Some(builder) = context.report_lint(&INVALID_IMPLEMENTATION, member_name)
        {
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "`{}` declares no member `{member_name}`",
                interface_instance.display(db),
            ));
            diagnostic.help("use an `extension` to add members that are not part of the interface");
        }
    }
}

/// the declared return type of `function`, as the conversion machinery sees it.
///
/// Both sides go through this: the transpiler to find what a `return` value must
/// convert to, and the checker to confirm the type it is enforcing is the same one
/// — a `return` is only a conversion site when the two agree, so the lowering can
/// never be handed a target the checker did not use.
pub(crate) fn function_declared_return_type<'db>(
    db: &'db dyn Db,
    file: File,
    function: &ast::StmtFunctionDef,
) -> Option<Type<'db>> {
    let index = semantic_index(db, file);
    let definition = index.expect_single_definition(function);
    let Type::FunctionLiteral(literal) = crate::types::binding_type(db, definition) else {
        return None;
    };
    // one signature only: an overloaded function has no single declared return
    let [overload] = literal.signature(db).overloads.as_slice() else {
        return None;
    };
    Some(overload.return_ty)
}

/// the element expressions of a collection literal, when each one is a whole
/// expression the transpiler can wrap.
///
/// `None` for anything else — including a literal containing an unpack
/// (`[*bs]`, `{**d}`), whose elements come from another collection and so have no
/// expression of their own at this site
pub(crate) fn addressable_elements(value: &ast::Expr) -> Option<Vec<&ast::Expr>> {
    fn plain(elements: &[ast::Expr]) -> Option<Vec<&ast::Expr>> {
        elements
            .iter()
            .map(|element| (!element.is_starred_expr()).then_some(element))
            .collect()
    }
    match value {
        ast::Expr::List(list) => plain(&list.elts),
        ast::Expr::Set(set) => plain(&set.elts),
        ast::Expr::Tuple(tuple) => plain(&tuple.elts),
        // only the values convert; a key's own type is checked against the key type
        ast::Expr::Dict(dict) => dict
            .items
            .iter()
            .map(|item| item.key.as_ref().map(|_| &item.value))
            .collect(),
        ast::Expr::ListComp(comp) => Some(vec![&comp.elt]),
        ast::Expr::SetComp(comp) => Some(vec![&comp.elt]),
        ast::Expr::Generator(comp) => Some(vec![&comp.elt]),
        ast::Expr::DictComp(comp) => Some(vec![&comp.value]),
        _ => None,
    }
}

/// the type a declared collection's elements must satisfy: a mapping's *value*
/// type, else what iterating the declared type yields
pub(crate) fn declared_element_type<'db>(
    db: &'db dyn Db,
    declared: Type<'db>,
) -> Option<Type<'db>> {
    // a mapping is keyed, and only its values sit at the literal's element
    // positions (`{"k": b}`); iterating one would give the *key* type
    if let Some((_, value)) = declared.unpack_keys_and_items(db) {
        return Some(value);
    }
    let element = declared.iterate(db).homogeneous_element_type(db);
    (!element.is_unknown() && !element.is_never()).then_some(element)
}

/// the name of the class a witness lowers to: the `as` name of a named
/// implementation, else a mangled `_by_impl__<Interface>__<Implemented>`.
///
/// the transpiler asks for this rather than deriving it, so that the class it
/// emits and the constructor it inserts at a conversion site can never disagree.
/// `None` when either side of the header is unresolved — the declaration is
/// already an error, and a placeholder name would collide with every other
/// unresolved implementation in the module
pub(crate) fn witness_class_name<'db>(
    db: &'db dyn Db,
    implementation: StaticClassLiteral<'db>,
) -> Option<String> {
    // a named implementation's witness *is* the class, so its own name is the
    // `as` name the header spelled
    if *implementation.witness_is_named(db) {
        return Some(implementation.name(db).to_string());
    }
    let base = witness_base_name(db, implementation)?;
    // two interfaces can share a short name (`a.Show` and `b.Show`); an ordinal
    // keeps their witnesses apart, counted the same way on both sides
    let ordinal = implementations_in_module(db, implementation.file(db))
        .iter()
        .take_while(|candidate| **candidate != implementation)
        .filter(|candidate| witness_base_name(db, **candidate).as_deref() == Some(base.as_str()))
        .count();
    if ordinal == 0 {
        Some(base)
    } else {
        Some(format!("{base}__{}", ordinal + 1))
    }
}

/// the un-ordinalized mangled name, used to count same-name collisions. `None`
/// when either side of the header is unresolved
fn witness_base_name<'db>(
    db: &'db dyn Db,
    implementation: StaticClassLiteral<'db>,
) -> Option<String> {
    let interface = implemented_interface(db, implementation)?
        .class_literal(db)
        .name(db)
        .to_string();
    let implemented = implementation
        .implemented_type_name(db)
        .as_ref()?
        .to_string();
    // one leading underscore, not two: python name-mangles a `__name` reference
    // inside a class body (`__by_impl__A__B` → `_Holder__by_impl__A__B`), which
    // would break every conversion that appears in one
    Some(format!("_by_impl__{interface}__{implemented}"))
}

/// `witness_class_name` for a class literal the transpiler holds: `None` when
/// the class is not a witness at all
pub fn witness_class_name_of<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> Option<String> {
    let ClassLiteral::Static(static_literal) = class else {
        return None;
    };
    if !static_literal.is_implementation(db) {
        return None;
    }
    witness_class_name(db, static_literal)
}

/// the annotated parameter type each of `call`'s source-order arguments was
/// matched to, binding the call outside inference.
///
/// `None` when the callee is a union or overloaded — where no single parameter
/// type per argument is well-defined. The checker declines to repair in exactly
/// that case too, so the two answers cannot drift apart
pub(crate) fn call_parameter_types<'db>(
    model: &crate::semantic_model::SemanticModel<'db>,
    callable_ty: Type<'db>,
    call: &ast::ExprCall,
) -> Option<Vec<Option<Type<'db>>>> {
    use crate::types::call::CallArguments;
    use crate::types::constraints::ConstraintSetBuilder;

    let db = model.db();
    let arguments = CallArguments::from_arguments_typed(&call.arguments, |splatted_value| {
        crate::HasType::inferred_type(splatted_value, model).unwrap_or_else(Type::unknown)
    });
    let constraints = ConstraintSetBuilder::new();
    // a conversion site's binding *does* carry an argument error — the checker
    // suppresses its diagnostic rather than making the argument assignable — so
    // the parameter types have to be read out of either outcome
    let bindings = match callable_ty
        .bindings(db)
        .match_parameters(db, &arguments)
        .check_types(
            db,
            &constraints,
            &arguments,
            crate::types::TypeContext::default(),
            &[],
        ) {
        Ok(bindings) => bindings,
        Err(error) => *error.into_bindings(),
    };
    bindings.plain_callee_parameter_types(arguments.len())
}

/// how the transpiler materializes a witness at one conversion site
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImplementationConversion {
    /// the witness class to construct
    pub witness: String,
    /// the module to import the witness class from, when the implementation is
    /// declared in a module other than the one being transpiled
    pub import_from: Option<String>,
}

/// build the transpiler's view of a repair
pub(crate) fn conversion_info<'db>(
    db: &'db dyn Db,
    from_file: File,
    repair: &ImplementationRepair<'db>,
) -> Option<ImplementationConversion> {
    // an ambiguous site is an error the checker reports; emitting either witness
    // would be picking one arbitrarily
    if repair.ambiguous_with.is_some() {
        return None;
    }
    let witness_file = repair.witness.file(db);
    let import_from = if witness_file == from_file {
        None
    } else {
        // spelled the way this file already imports the module — an absolute module
        // name the interpreter cannot resolve would break at runtime
        Some(imported_module_spelling(db, from_file, witness_file)?)
    };
    Some(ImplementationConversion {
        witness: witness_class_name(db, repair.witness)?,
        import_from,
    })
}

/// the type a witness constructor takes: `__init__(self, implemented: B)`.
///
/// a witness has no state of its own, so this is its whole signature — and it is
/// how a named implementation is called explicitly (`BAsA(b)`)
pub(crate) fn witness_constructor_parameter<'db>(
    db: &'db dyn Db,
    witness: StaticClassLiteral<'db>,
) -> Option<Type<'db>> {
    let implemented = implemented_view_class(db, witness)?;
    Some(Type::instance(db, implemented))
}

/// does `class_node`'s interface permit an implementation at all? an interface
/// must be abstract or a protocol: implementing a concrete class would mean
/// promising fields the witness cannot hold
pub(crate) fn interface_is_implementable<'db>(db: &'db dyn Db, interface: ClassType<'db>) -> bool {
    interface.class_literal(db).is_protocol(db) || is_abstract(db, interface)
}

/// is this implementation declared at module level? its body scope's parent must
/// be the module scope — a block inside a function or a class body is neither
/// enumerable by the registry nor lowered by the transpiler
fn declared_at_module_level<'db>(db: &'db dyn Db, implementation: StaticClassLiteral<'db>) -> bool {
    let file = implementation.file(db);
    let body_scope = implementation.body_scope(db);
    semantic_index(db, file)
        .ancestor_scopes(body_scope.file_scope_id(db))
        .nth(1)
        .is_none_or(|(scope_id, _)| scope_id.is_global())
}

/// does the interface itself declare `name`, rather than inheriting it from
/// `object`? `object`'s versions are the ones a witness may safely replace with
/// delegating ones
fn interface_declares<'db>(db: &'db dyn Db, interface: ClassType<'db>, name: &str) -> bool {
    !Type::instance(db, interface)
        .member_lookup_with_policy(
            db,
            name,
            crate::types::MemberLookupPolicy::MRO_NO_OBJECT_FALLBACK,
        )
        .place
        .is_undefined()
}

/// the delegating dunders a witness class may carry: those the interface leaves
/// to `object`.
///
/// A witness and the object it wraps should be interchangeable as dict keys, in
/// sets, and in `repr` output — but only where the interface has no opinion. When
/// the interface defines one of these itself, its version must win, so the
/// witness must not shadow it. `__eq__` and `__hash__` move together: python sets
/// `__hash__ = None` on any class that defines `__eq__` alone, which would make
/// the witness unhashable and clobber an interface-provided `__hash__`.
pub fn witness_delegated_dunders<'db>(
    db: &'db dyn Db,
    witness: ClassLiteral<'db>,
) -> Vec<&'static str> {
    let ClassLiteral::Static(static_literal) = witness else {
        return Vec::new();
    };
    let Some(interface) = implemented_interface(db, static_literal) else {
        return Vec::new();
    };
    let mut delegated = Vec::new();
    if !interface_declares(db, interface, "__eq__")
        && !interface_declares(db, interface, "__hash__")
    {
        delegated.push("__eq__");
        delegated.push("__hash__");
    }
    if !interface_declares(db, interface, "__repr__") {
        delegated.push("__repr__");
    }
    delegated
}

/// is `class` an abstract class — one that still has an unimplemented abstract
/// method, or whose metaclass is `ABCMeta`?
fn is_abstract<'db>(db: &'db dyn Db, class: ClassType<'db>) -> bool {
    if !class.abstract_methods(db).is_empty() {
        return true;
    }
    class
        .metaclass(db)
        .to_class_type(db)
        .is_some_and(|metaclass| {
            metaclass
                .class_literal(db)
                .is_known(db, KnownClass::ABCMeta)
        })
}
