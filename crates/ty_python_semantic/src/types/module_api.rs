//! basedpython module api enforcement — the `implements` declaration
//!
//! A module is already a structural value: [`Type::ModuleLiteral`] answers a
//! protocol's members through the module's public surface, so
//! `backend: Backend = postgres` type-checks on its own. What this module adds is
//! a way to attach that obligation to a module *permanently*, so it is checked in
//! the file that carries it rather than wherever someone happens to assign it —
//! or nowhere at all, when nobody does, which is the case for a plugin that is
//! only ever loaded by name.
//!
//! An obligation is attached in one of two ways, and both arrive at
//! [`module_obligations`]:
//!
//! - the module says so itself, with a bare `implements Backend`
//! - a package it lives in says so, with `implements Backend for ".*"` written in
//!   that package's `__init__`
//!
//! The second is what a plugin directory needs, and it is why obligations are
//! indexed by **containment**. A module has to be able to find the obligations
//! imposed on it, or the error would not appear in the file whose author can fix
//! it. Walking a module's ancestor packages costs a handful of lookups the module
//! resolver has already done. The two alternatives are worse: a rule anywhere in
//! the project would need a project-wide scan, making every file's check depend
//! on every file's declarations; and the import graph — the relation `extension`
//! visibility uses — is the wrong one here, because the whole point is imposing
//! on a module that does not import you.
//!
//! Containment also draws the ownership line in the right place: a package may
//! impose on modules inside itself, and nothing can reach into a package from
//! outside to add requirements to it.

use ruff_db::diagnostic::{Annotation, Span};
use ruff_db::files::FileRange;
use ruff_db::parsed::parsed_module;
use ruff_python_ast as ast;
use ruff_python_ast::helpers::{ImplementsDeclaration, implements_declaration};
use ruff_text_size::{Ranged, TextRange};
use ty_module_resolver::{
    ImportingFile, ModuleGlobSet, ModuleName, file_to_module, resolve_module,
};

use crate::Db;
use crate::place::global_symbol;
use crate::types::class::ClassType;
use crate::types::conformance::{interface_requirements, is_conformable};
use crate::types::context::InferContext;
use crate::types::diagnostic::{INVALID_MODULE_API, UNMET_MODULE_API};
use crate::types::{ProgramEnvironment, ProgramFile, Type};

/// An obligation on one module: an interface it has to answer, and every
/// declaration that asked for it.
///
/// One interface, however many declarations name it — a module that declares
/// `implements Backend` inside a package whose rule says the same thing has one
/// obligation, not two, because it has one thing left to do about it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
struct Obligation<'db> {
    /// the interface the module has to answer
    interface: ClassType<'db>,
    /// the module's own `implements`, when it declared this itself. The
    /// diagnostic is anchored here, since it is the line the reader can act on
    own: Option<TextRange>,
    /// every declaration that asked for this interface, the module's own
    /// included
    #[get_size(ignore)]
    sources: Box<[FileRange]>,
}

impl Obligation<'_> {
    /// Where a diagnostic about this obligation is anchored: the module's own
    /// declaration, or the top of the file when a package imposed it and there is
    /// nothing in the module to point at.
    fn anchor(&self) -> TextRange {
        self.own.unwrap_or_default()
    }
}

/// Collects obligations, merging the declarations that name the same interface.
#[derive(Default)]
struct Obligations<'db> {
    collected: Vec<Obligation<'db>>,
}

impl<'db> Obligations<'db> {
    fn add(&mut self, interface: ClassType<'db>, source: FileRange, own: Option<TextRange>) {
        if let Some(existing) = self
            .collected
            .iter_mut()
            .find(|obligation| obligation.interface == interface)
        {
            existing.own = existing.own.or(own);
            let mut sources = existing.sources.to_vec();
            sources.push(source);
            existing.sources = sources.into_boxed_slice();
            return;
        }
        self.collected.push(Obligation {
            interface,
            own,
            sources: Box::from([source]),
        });
    }
}

/// A rule a package's `__init__` imposes on the modules in its subtree.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
struct PackageRule<'db> {
    interface: ClassType<'db>,
    /// the patterns exactly as written, still relative to the declaring package
    patterns: Box<[Box<str>]>,
    /// the `implements` keyword the rule is written at
    range: TextRange,
}

/// The rules `file` imposes on its subtree, if it is a package `__init__` that
/// declares any.
///
/// Cycles for the reason the conformance registry does: resolving the interface a
/// rule names infers the declaring module's code, and a package `__init__`
/// routinely imports the very submodules its rules govern. Recovery is the same —
/// no rules yet.
#[salsa::tracked(
    returns(deref),
    cycle_initial = |_, _, _| Box::default(),
    heap_size = ruff_memory_usage::heap_size
)]
fn package_rules<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> Box<[PackageRule<'db>]> {
    if !file.file(db).source_type(db).is_basedpython() {
        return Box::default();
    }
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let mut rules = Vec::new();
    for statement in &parsed.syntax().body {
        let Some(declaration) = implements_declaration(statement) else {
            continue;
        };
        if declaration.patterns.is_empty() {
            continue;
        }
        let patterns: Box<[Box<str>]> = declaration
            .patterns
            .iter()
            .filter_map(|pattern| pattern.as_string_literal_expr())
            .map(|pattern| Box::from(pattern.value.to_str()))
            .collect();
        for interface in declaration.interfaces {
            let Some(interface) = resolve_interface(db, file, interface) else {
                continue;
            };
            if !is_conformable(db, interface) {
                continue;
            }
            rules.push(PackageRule {
                interface,
                patterns: patterns.clone(),
                range: declaration.keyword_range,
            });
        }
    }
    rules.into_boxed_slice()
}

/// The module `file` belongs to, as importers see it.
///
/// Stub-preferred, so for a module with both a `.by` and a `.byi` this is the
/// stub — the file whose surface everything outside the module reads, and
/// therefore the only surface an obligation can sensibly be about.
fn api_module<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> Option<ty_module_resolver::Module<'db>> {
    let module = file_to_module(db, file.resolver_file(db))?;
    resolve_module(
        db,
        ImportingFile::File(file.file(db), file.resolver_environment(db)),
        module.name(db),
    )
}

/// Is `file` the file that *is* its module's api?
///
/// False for an implementation file shadowed by a stub. Both files are checked by
/// the project, so without this the same obligation would be reported twice, once
/// against a surface nobody outside the module can see.
fn is_api_file<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> bool {
    api_module(db, file)
        .and_then(|module| module.file(db))
        .is_some_and(|api| api == file.file(db))
}

/// Every obligation that applies to `file`'s module: the ones it declares itself,
/// and the ones its ancestor packages impose on it.
#[salsa::tracked(
    returns(deref),
    cycle_initial = |_, _, _| Box::default(),
    heap_size = ruff_memory_usage::heap_size
)]
fn module_obligations<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> Box<[Obligation<'db>]> {
    let Some(module) = file_to_module(db, file.resolver_file(db)) else {
        return Box::default();
    };
    // a rule cannot reach outside its own package, and nothing anyone can fix
    // lives outside the project, so checking installed or vendored code would
    // only produce noise
    if module
        .search_path(db)
        .is_none_or(|path| !path.is_first_party())
    {
        return Box::default();
    }

    // an implementation shadowed by a stub is not the module's api, and checking
    // it too would report the same obligation twice, once against a surface
    // nobody outside the module can see. `check_declarations` is what tells the
    // author their declaration is in the wrong file
    if !is_api_file(db, file) {
        return Box::default();
    }

    let mut obligations = Obligations::default();

    // the module's own declarations. a `for` clause on one of these is a rule,
    // handled by `package_rules` for the modules it names, and says nothing about
    // the file it is written in
    if file.file(db).source_type(db).is_basedpython() {
        let parsed = parsed_module(db, file.python_file(db)).load(db);
        for statement in &parsed.syntax().body {
            let Some(declaration) = implements_declaration(statement) else {
                continue;
            };
            if !declaration.patterns.is_empty() {
                continue;
            }
            for interface in declaration.interfaces {
                let Some(interface) = resolve_interface(db, file, interface) else {
                    continue;
                };
                if !is_conformable(db, interface) {
                    continue;
                }
                obligations.add(
                    interface,
                    FileRange::new(file.file(db), declaration.keyword_range),
                    Some(declaration.keyword_range),
                );
            }
        }
    }

    let name = module.name(db);
    for ancestor in ancestor_packages(name) {
        let Some(package) = resolve_module(
            db,
            ImportingFile::File(file.file(db), file.resolver_environment(db)),
            &ancestor,
        ) else {
            continue;
        };
        let Some(package_file) = package
            .file(db)
            .map(|package_file| ProgramFile::new(db, package_file, file.program(db)))
        else {
            continue;
        };
        for rule in package_rules(db, package_file) {
            if !Reach::new(&ancestor, &rule.patterns).includes(&ancestor, name) {
                continue;
            }
            obligations.add(
                rule.interface,
                FileRange::new(package_file.file(db), rule.range),
                None,
            );
        }
    }

    obligations.collected.into_boxed_slice()
}

/// The strict ancestor packages of `name`, outermost first.
///
/// Strict, so a package's own rules never oblige the package itself: patterns are
/// relative to the declaring package and name what is *inside* it.
fn ancestor_packages(name: &ModuleName) -> Vec<ModuleName> {
    let components: Vec<&str> = name.components().collect();
    (1..components.len())
        .filter_map(|end| ModuleName::new(&components[..end].join(".")))
        .collect()
}

/// The compiled reach of one rule: which modules its patterns include.
struct Reach {
    /// every pattern the rule wrote
    all: ModuleGlobSet,
    /// only the include patterns whose last component is a literal name, which is
    /// what decides whether a private module was reached deliberately
    named: ModuleGlobSet,
}

impl Reach {
    /// A pattern that cannot be resolved against the package that wrote it is
    /// skipped rather than poisoning the rule: [`check_patterns`] reports it, and
    /// the patterns beside it go on meaning what they say. A rule whose patterns
    /// are *all* unusable reaches nothing, which is what it says.
    fn new(package: &ModuleName, patterns: &[Box<str>]) -> Self {
        let absolute: Vec<String> = patterns
            .iter()
            .filter_map(|pattern| absolute_pattern(package, pattern))
            .collect();
        // a pattern reaches a private module only by naming it outright, so the
        // ones that can are those with no wildcard anywhere
        let named: Vec<String> = patterns
            .iter()
            .filter(|pattern| !pattern.starts_with('!') && !pattern.contains('*'))
            .filter_map(|pattern| absolute_pattern(package, pattern))
            .collect();
        Self {
            all: Self::compile(&absolute),
            named: Self::compile(&named),
        }
    }

    /// An invalid pattern is [`check_patterns`]' to report; a set that will not
    /// compile simply matches nothing.
    fn compile(patterns: &[String]) -> ModuleGlobSet {
        ModuleGlobSet::from_patterns(patterns.iter().map(String::as_str))
            .unwrap_or_else(|_| ModuleGlobSet::empty())
    }

    fn includes(&self, package: &ModuleName, name: &ModuleName) -> bool {
        if !self.all.matches(name).is_include() {
            return false;
        }
        // a leading underscore already means "not part of the surface" everywhere
        // else, and a private helper sitting among the plugins is the common case.
        // every component below the declaring package counts, so a wildcard does
        // not reach into a private *package* either
        let private = name
            .components()
            .skip(package.components().count())
            .any(|component| component.starts_with('_'));
        if !private {
            return true;
        }
        self.named.matches(name).is_include()
    }
}

/// Whether a rule reaches something in its package — or whether that cannot be
/// answered, which counts the same.
///
/// The whole subtree, not just the direct submodules: a pattern may name a
/// subpackage's contents (`".deep.*"`).
///
/// A package with no submodules the resolver can enumerate is treated as
/// reaching something, because there is no evidence either way. That covers a
/// namespace portion inside the package, whose contents span directories and
/// search paths and are deliberately not enumerated
/// ([`ty_module_resolver::Module::all_submodules`] drops them). Accusing a rule
/// that does enforce is worse than missing a typo.
fn reaches_any<'db>(
    db: &'db dyn Db,
    package_module: ty_module_resolver::Module<'db>,
    package: &ModuleName,
    patterns: &[Box<str>],
) -> bool {
    reaches_subtree(db, package_module, package, &Reach::new(package, patterns))
}

fn reaches_subtree<'db>(
    db: &'db dyn Db,
    package_module: ty_module_resolver::Module<'db>,
    package: &ModuleName,
    reach: &Reach,
) -> bool {
    let submodules = package_module.all_submodules(db);
    if submodules.is_empty() {
        return package_module.kind(db).is_package();
    }
    submodules.iter().any(|&submodule| {
        reach.includes(package, submodule.name(db))
            || reaches_subtree(db, submodule, package, reach)
    })
}

/// How a pattern that is not relative should have been written.
///
/// A pattern that spells the package out in full (`"pkg.*"` inside `pkg`) is the
/// common mistake, and its fix is not the same as an absolute pattern's.
fn relative_spelling(package: &ModuleName, written: &str) -> String {
    let (negation, body) = match written.strip_prefix('!') {
        Some(body) => ("!", body),
        None => ("", written),
    };
    let inside = body.strip_prefix(&format!("{package}.")).unwrap_or(body);
    format!("{negation}.{}", inside.trim_start_matches('.'))
}

/// A pattern as written, resolved against the package that declared it.
///
/// `None` for a pattern that is not relative to its package — an absolute one, or
/// one that tries to climb — which [`check_module_api`] reports at the rule.
fn absolute_pattern(package: &ModuleName, pattern: &str) -> Option<String> {
    let (negation, body) = match pattern.strip_prefix('!') {
        Some(body) => ("!", body),
        None => ("", pattern),
    };
    if !body.starts_with('.') || body.starts_with("..") {
        return None;
    }
    Some(format!("{negation}{package}{body}"))
}

/// The interface an `implements` declaration names, resolved in the file that
/// wrote it.
///
/// By name rather than by inferring the expression, because a rule is read from a
/// *different* file than the one being checked, where there is no inference of
/// that file's expressions to read a type out of.
fn resolve_interface<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    expression: &ast::Expr,
) -> Option<ClassType<'db>> {
    resolve_reference(db, file, expression)?.to_class_type(db)
}

/// The value a name or dotted name refers to, in the file that wrote it.
fn resolve_reference<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    expression: &ast::Expr,
) -> Option<Type<'db>> {
    match expression {
        ast::Expr::Name(name) => global_symbol(db, file, name.id.as_str())
            .place
            .ignore_possibly_undefined(),
        ast::Expr::Attribute(attribute) => {
            let value = resolve_reference(db, file, &attribute.value)?;
            let env = ProgramEnvironment::from_file(file);
            value
                .member(db, &env, attribute.attr.id.as_str())
                .place
                .ignore_possibly_undefined()
        }
        _ => None,
    }
}

/// Checks a module's `implements` declarations, and the obligations that apply to
/// it, at the end of inferring its module scope.
///
/// Two directions, deliberately reported in different files. A declaration that
/// is malformed is the declaring file's problem, so it is reported there; an
/// obligation the module fails to answer is the *obliged* module's problem, even
/// when a package imposed it, so it is reported in the module — with a secondary
/// annotation on the rule, because an error saying a module must have `connect`
/// is useless without saying who says so.
pub(super) fn check_module_api<'ast>(context: &InferContext<'_, 'ast>, body: &'ast [ast::Stmt]) {
    let db = context.db();
    let file = context.program_file();
    // an obligation cannot reach outside its own package, and a diagnostic about
    // a file the project does not check is never shown — so doing any of this for
    // installed or vendored code is pure waste
    if file_to_module(db, file.resolver_file(db))
        .and_then(|module| module.search_path(db))
        .is_none_or(|path| !path.is_first_party())
    {
        return;
    }
    // an experimental feature is off unless the project asked for it. a
    // declaration written while it is off is *reported*, not ignored: an
    // obligation nothing checks is the failure mode this whole feature exists to
    // remove, and it would be a strange one to introduce here
    if !db.experimental_settings().module_api {
        if file.file(db).source_type(db).is_basedpython() {
            report_disabled_declarations(context, body);
        }
        return;
    }

    if file.file(db).source_type(db).is_basedpython() {
        if !is_api_file(db, file) {
            report_shadowed_declarations(context, body);
            return;
        }
        check_declarations(context, body);
        report_misplaced_declarations(context, body);
    }
    check_obligations(context);
}

/// Reports every declaration in a project that has not opted in to the feature.
fn report_disabled_declarations<'ast>(context: &InferContext<'_, 'ast>, body: &'ast [ast::Stmt]) {
    for statement in body {
        let Some(declaration) = implements_declaration(statement) else {
            continue;
        };
        let Some(builder) = context.report_lint(&INVALID_MODULE_API, declaration.keyword_range)
        else {
            continue;
        };
        let mut diagnostic =
            builder.into_diagnostic("`implements` is an experimental feature, and is off");
        diagnostic.info("nothing is checked against this declaration until the project opts in");
        diagnostic.help(
            "Enable it with `module-api = true` under `[experimental]` in `basedpython.toml`",
        );
    }
}

/// Reports a declaration written in an implementation file that a stub shadows.
///
/// Nothing outside the module reads that file's surface, so an obligation
/// attached there would be about a surface that does not exist as far as anyone
/// else is concerned — and the stub, which *is* the module's api, would go
/// unchecked. Rather than check the wrong thing quietly, say where it belongs.
fn report_shadowed_declarations<'ast>(context: &InferContext<'_, 'ast>, body: &'ast [ast::Stmt]) {
    let db = context.db();
    for statement in body {
        let Some(declaration) = implements_declaration(statement) else {
            continue;
        };
        let Some(builder) = context.report_lint(&INVALID_MODULE_API, declaration.keyword_range)
        else {
            continue;
        };
        let mut diagnostic = builder
            .into_diagnostic("this module's api is its stub, so its declarations belong there");
        if let Some(stub) =
            api_module(db, context.program_file()).and_then(|module| module.file(db))
        {
            diagnostic.info(format_args!(
                "everything outside this module reads `{}`",
                stub.path(db)
            ));
        }
        diagnostic.help("Move the declaration into the stub");
    }
}

/// Reports what is wrong with the declarations `file` itself writes.
fn check_declarations<'ast>(context: &InferContext<'_, 'ast>, body: &'ast [ast::Stmt]) {
    let db = context.db();
    let file = context.program_file();
    let module = file_to_module(db, file.resolver_file(db));
    let is_package = module.is_some_and(|module| module.kind(db).is_package());

    for statement in body {
        let Some(declaration) = implements_declaration(statement) else {
            continue;
        };

        for interface in declaration.interfaces {
            // an interface that does not resolve, or resolves to nothing usable —
            // an undefined name, an unresolved import — has already been reported
            // on the name itself, and saying it twice helps nobody
            let Some(resolved) = resolve_reference(db, file, interface) else {
                continue;
            };
            if resolved.is_dynamic() {
                continue;
            }
            let class = resolved.to_class_type(db);
            if class.is_some_and(|class| is_conformable(db, class)) {
                continue;
            }
            if let Some(builder) = context.report_lint(&INVALID_MODULE_API, interface) {
                let mut diagnostic = builder.into_diagnostic(match class {
                    Some(class) => format!("`{}` is not a protocol", class.name(db)),
                    None => format!(
                        "`{}` is not a protocol",
                        resolved.display(db, context.program_environment())
                    ),
                });
                diagnostic.info(
                    "a module answers an interface through its public surface, which only a \
                     protocol describes",
                );
            }
        }

        if declaration.patterns.is_empty() {
            continue;
        }

        // a rule is found by walking a module's ancestor packages, so a rule
        // written anywhere else would govern nothing and silently enforce nothing
        if !is_package
            && let Some(builder) =
                context.report_lint(&INVALID_MODULE_API, declaration.keyword_range)
        {
            let mut diagnostic = builder.into_diagnostic(
                "a `for` clause may only be written in a package's `__init__`".to_string(),
            );
            diagnostic.info(
                "a module finds the rules imposed on it by walking the packages it is in, so a \
                 rule anywhere else would reach nothing",
            );
            diagnostic.help("Move the declaration into the package's `__init__`, or drop the `for` clause to oblige this module itself");
        }

        if let (true, Some(module)) = (is_package, module) {
            check_patterns(context, &declaration, module.name(db));
        }
    }
}

/// Reports the patterns of one rule that cannot be resolved against the package
/// that wrote them, and the rule that reaches nothing.
fn check_patterns<'ast>(
    context: &InferContext<'_, 'ast>,
    declaration: &ImplementsDeclaration<'ast>,
    package: &ModuleName,
) {
    let db = context.db();
    let mut patterns = Vec::new();
    for pattern in declaration.patterns {
        let Some(literal) = pattern.as_string_literal_expr() else {
            continue;
        };
        let written = literal.value.to_str();
        let Some(absolute) = absolute_pattern(package, written) else {
            if let Some(builder) = context.report_lint(&INVALID_MODULE_API, pattern) {
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "`{written}` is not a pattern relative to `{package}`"
                ));
                diagnostic.info(
                    "a rule reaches into the package it is written in, so its patterns start \
                     with a `.`, as a relative import does",
                );
                // a pattern with nothing left once the package prefix comes off
                // has no spelling to suggest — `"."` is not one
                let spelling = relative_spelling(package, written);
                if spelling.trim_start_matches('!') != "." {
                    diagnostic.help(format_args!("Write `\"{spelling}\"`"));
                }
            }
            continue;
        };
        if let Err(error) = ModuleGlobSet::from_patterns([absolute.as_str()]) {
            if let Some(builder) = context.report_lint(&INVALID_MODULE_API, pattern) {
                builder.into_diagnostic(format_args!(
                    "`{written}` is not a valid module pattern: {error}"
                ));
            }
            continue;
        }
        patterns.push(Box::from(written));
    }

    // a pattern that reaches nothing enforces nothing, silently, which is the
    // worst thing a check can do. the package's own subtree answers this, so it
    // costs a walk of the package rather than one of the project
    if patterns.is_empty() {
        return;
    }
    let Some(package_module) = resolve_module(
        db,
        ImportingFile::File(
            context.file(),
            context.program_file().resolver_environment(db),
        ),
        package,
    ) else {
        return;
    };
    let reaches_something = reaches_any(db, package_module, package, &patterns);
    if !reaches_something
        && let Some(builder) = context.report_lint(&INVALID_MODULE_API, declaration.keyword_range)
    {
        let mut diagnostic = builder.into_diagnostic("this rule reaches no module".to_string());
        diagnostic.info(format_args!(
            "nothing in `{package}` matches, so the declaration obliges nothing"
        ));
    }
}

/// Reports an `implements` written somewhere other than module level.
///
/// An obligation is about a module's surface, so a declaration inside a function
/// or a class body has nothing to attach to — and nothing looks for one there.
fn report_misplaced_declarations<'ast>(context: &InferContext<'_, 'ast>, body: &'ast [ast::Stmt]) {
    struct Nested<'a, 'db, 'ast> {
        context: &'a InferContext<'db, 'ast>,
    }

    impl<'ast> ruff_python_ast::statement_visitor::StatementVisitor<'ast> for Nested<'_, '_, 'ast> {
        fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
            if let Some(declaration) = implements_declaration(stmt)
                && let Some(builder) = self
                    .context
                    .report_lint(&INVALID_MODULE_API, declaration.keyword_range)
            {
                let mut diagnostic = builder.into_diagnostic(
                    "an `implements` declaration belongs at module level".to_string(),
                );
                diagnostic.info(
                    "an obligation is about a module's public surface, so there is nothing for \
                     one written inside a body to attach to",
                );
            }
            ruff_python_ast::statement_visitor::walk_stmt(self, stmt);
        }
    }

    let mut visitor = Nested { context };
    for statement in body {
        // the module's own statements are the well-placed ones; only what is
        // nested inside them is misplaced
        if implements_declaration(statement).is_none() {
            ruff_python_ast::statement_visitor::walk_stmt(&mut visitor, statement);
        }
    }
}

/// Checks the module against every obligation that applies to it.
fn check_obligations(context: &InferContext<'_, '_>) {
    let db = context.db();
    let env = context.program_environment();
    let file = context.program_file();
    let obligations = module_obligations(db, file);
    if obligations.is_empty() {
        return;
    }

    // the module as importers see it: with a stub, that is the stub's surface,
    // which is the only one an obligation can be about
    let Some(module) = api_module(db, file) else {
        return;
    };
    let module_ty = Type::module_literal(db, file, module);

    // a module-level `__getattr__` answers every name, so every requirement would
    // be met vacuously. a silent pass is worse than no check at all
    let has_getattr = global_symbol(db, file, "__getattr__")
        .place
        .ignore_possibly_undefined()
        .is_some();

    for obligation in obligations {
        let interface = obligation.interface;
        let interface_instance = Type::instance(db, env, interface);

        if has_getattr {
            if let Some(builder) = context.report_lint(&INVALID_MODULE_API, obligation.anchor()) {
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "`{}` cannot be checked against `{}`",
                    module.name(db),
                    interface_instance.display(db, env),
                ));
                diagnostic.info(
                    "this module's `__getattr__` answers every name, so every requirement would \
                     be met without anything being defined",
                );
            }
            continue;
        }

        if module_ty.is_assignable_to(db, env, interface_instance) {
            continue;
        }

        let mut missing = Vec::new();
        let mut mismatched = Vec::new();
        for requirement in interface_requirements(db, interface) {
            let name = requirement.as_str();
            let Some(expected) = interface_instance
                .member(db, env, name)
                .place
                .ignore_possibly_undefined()
            else {
                continue;
            };
            match module_ty
                .member(db, env, name)
                .place
                .ignore_possibly_undefined()
            {
                None => missing.push(requirement.clone()),
                Some(actual) => {
                    if !actual.is_assignable_to(db, env, expected) {
                        mismatched.push((requirement.clone(), expected, actual));
                    }
                }
            }
        }

        let Some(builder) = context.report_lint(&UNMET_MODULE_API, obligation.anchor()) else {
            continue;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "`{}` does not answer `{}`",
            module.name(db),
            interface_instance.display(db, env),
        ));
        for name in &missing {
            diagnostic.info(format_args!("`{name}` is missing"));
        }
        for (name, expected, actual) in &mismatched {
            diagnostic.info(format_args!(
                "`{name}` is `{}`, but `{}` declares it as `{}`",
                actual.display(db, env),
                interface_instance.display(db, env),
                expected.display(db, env),
            ));
        }
        if missing.is_empty() && mismatched.is_empty() {
            diagnostic.info(format_args!(
                "`{}` is not assignable to `{}`",
                module_ty.display(db, env),
                interface_instance.display(db, env),
            ));
        }
        for source in &obligation.sources {
            // the module's own declaration is already the primary annotation
            if Some(source.range()) == obligation.own && source.file() == context.file() {
                continue;
            }
            diagnostic.annotate(
                Annotation::secondary(Span::from(*source)).message("required by this declaration"),
            );
        }
    }
}
