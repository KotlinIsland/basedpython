#![warn(
    clippy::disallowed_methods,
    reason = "Prefer System trait methods over std methods in ty crates"
)]
use crate::lint::{LintRegistry, LintRegistryBuilder};
use crate::suppression::{
    BLANKET_IGNORE_COMMENT, IGNORE_COMMENT_UNKNOWN_RULE, INVALID_IGNORE_COMMENT,
    UNUSED_TYPE_IGNORE_COMMENT,
};
use crate::types::check_types_with;
pub use db::Db;
pub(crate) use diagnostic::add_inferred_python_version_hint_to_diagnostic;
pub use diagnostic::inferred_python_version_source_annotation;
pub use fixes::{fix_all_diagnostics, suppress_all_diagnostics};
pub use place::{
    basedpython_typing_added_in, basedpython_warnings_added_in, basedpython_warnings_symbol,
};
pub use preset::TypeCheckingPreset;
use ruff_db::PythonFile;
use ruff_db::diagnostic::{Annotation, Diagnostic, DiagnosticId, Severity, Span};
use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_db::source::{SourceTextError, source_text};
use rustc_hash::FxHasher;
pub use semantic_model::{
    Completion, DjangoLookupArgument, ExpectedStringLiteralCompletion, ExtensionOperatorRewrite,
    HasDefinition, HasType, ImplicitReceiverReference, MemberDefinition, NameKind,
    PreludeDunderReceiver, SemanticModel,
};
use std::hash::BuildHasherDefault;
pub use suppression::UNUSED_IGNORE_COMMENT;
pub use suppression::suppress_single;
pub(crate) use suppression::{SuppressFix, is_unused_ignore_comment_lint, suppress_all};
use ty_module_resolver::ModuleGlobSet;
pub use ty_python_core::Program;
use ty_python_core::ProgramFile;
use ty_python_core::definition::docstring_from_body;
use ty_python_core::platform::PythonPlatform;
use ty_python_core::scope::ScopeId;
use ty_python_core::{
    BindingWithConstraintsIterator, DeclarationsIterator, FileScopeId, attribute_scopes,
    semantic_index,
};
pub use ty_site_packages::{
    PythonEnvironment, PythonVersionFileSource, PythonVersionSource, PythonVersionWithSource,
    SitePackagesPaths, SysPrefixPathOrigin,
};
pub use types::conformance::declares_conformances;
pub use types::conformance::{
    ConformanceRegistration, ConformanceTest, WitnessDispatch, WitnessKind,
};
pub use types::conversions::{
    ConversionImport, ConversionInfo, ConversionRuntime, DISCARD_ADAPTER,
};
pub use types::extensions::{ExtensionAttributeInfo, ExtensionMemberKind};
pub use types::ide_support::{
    ImplementationsFinder, ImportAliasResolution, OverridableMember, ResolvedDefinition,
    TypeHierarchyClass, contains_identifier, definitions_for_attribute, definitions_for_bin_op,
    definitions_for_django_lookup_root, definitions_for_imported_symbol, definitions_for_name,
    definitions_for_unary_op, map_stub_definition, type_hierarchy_prepare, type_hierarchy_subtypes,
    type_hierarchy_supertypes,
};
pub use types::implicit_names::implicit_names;
pub use types::reified_infer::{
    ArgVariance, ErasedTargetReason, ErasedUnion, ParametricIsPlan, ProtocolMemberCheck,
};
pub use types::template::finite_string_set;
pub use types::visibility::private_symbols;
pub use types::{DisplaySettings, ProgramEnvironment, TypeQualifiers};

pub mod api_lockfile;
mod assumed;
pub use assumed::stop_offset;
mod db;
pub mod dependencies;
pub mod django_settings;
pub mod django_template;
mod dunder_all;
mod fixes;
pub mod lint;
pub(crate) mod place;
pub(crate) mod place_load;
mod preset;
mod reachability;
pub mod reified;
mod semantic_model;
mod subscript;
mod suppression;
pub mod types;

mod diagnostic;
#[cfg(feature = "testing")]
pub mod pull_types;

type FxOrderMap<K, V> = ordermap::map::OrderMap<K, V, BuildHasherDefault<FxHasher>>;
type FxOrderSet<V> = ordermap::set::OrderSet<V, BuildHasherDefault<FxHasher>>;
type FxIndexMap<K, V> = indexmap::IndexMap<K, V, BuildHasherDefault<FxHasher>>;
type FxIndexSet<V> = indexmap::IndexSet<V, BuildHasherDefault<FxHasher>>;

/// Returns the default registry with all known semantic lints.
pub fn default_lint_registry() -> &'static LintRegistry {
    static REGISTRY: std::sync::LazyLock<LintRegistry> = std::sync::LazyLock::new(|| {
        let mut registry = LintRegistryBuilder::default();
        register_lints(&mut registry);
        registry.build()
    });

    &REGISTRY
}

/// Register all known semantic lints.
fn register_lints(registry: &mut LintRegistryBuilder) {
    types::register_lints(registry);
    django_template::register_lints(registry);
    registry.register_lint(&UNUSED_IGNORE_COMMENT);
    registry.register_lint(&UNUSED_TYPE_IGNORE_COMMENT);
    registry.register_lint(&IGNORE_COMMENT_UNKNOWN_RULE);
    registry.register_lint(&INVALID_IGNORE_COMMENT);
    registry.register_lint(&BLANKET_IGNORE_COMMENT);
}

#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent analysis toggle; a state machine would not model them"
)]
pub struct AnalysisSettings {
    /// Whether narrowing with generic classes uses the top materialization.
    pub strict_generic_narrowing: bool,

    /// Whether ty should use conservative equality and inequality semantics.
    pub strict_equality_semantics: bool,

    /// Whether errors can be suppressed with `type: ignore` comments.
    ///
    /// If set to false, ty won't:
    ///
    /// * allow suppressing errors with `type: ignore` comments
    /// * report unused `type: ignore` comments
    /// * report invalid `type: ignore` comments
    pub respect_type_ignore_comments: bool,

    pub allowed_unresolved_imports: ModuleGlobSet,

    pub replace_imports_with_any: ModuleGlobSet,

    /// The requirement groups this file may import from, by name, or `None` to
    /// derive it from whether the file is part of what the project ships.
    ///
    /// `project` names `[project].dependencies` and `*` names every group. See
    /// [`crate::dependencies::available_groups`].
    pub dependency_groups: Option<Box<[Box<str>]>>,

    /// The top-level modules the project ships, or `None` to derive them from
    /// the name the project gives itself.
    ///
    /// Only a project that ships several unrelated modules needs to say.
    pub shipped_modules: Option<Box<[Box<str>]>>,

    /// The dependencies this project hands to its own users, which a build writes
    /// into the `by.typed` its package ships.
    ///
    /// Nothing about checking *this* project reads it: it is a statement to the
    /// projects that depend on this one, and they read it from the marker.
    pub exported_dependencies: Option<Box<[Box<str>]>>,

    /// Whether `float` and `complex` annotations mean *only* themselves, rather than
    /// admitting the wider numeric types the typing spec's special case allows.
    ///
    /// The special case says an `int` is acceptable wherever a `float` is asked for, so
    /// `x: float` really declares `int | float`. That is what a `.by` file opts out of
    /// already; this makes the same model available to a `.py` one, per module.
    ///
    /// It is not only a checking question. The wider annotation is why a `.py`
    /// `list[float]` cannot be laid out as an unboxed buffer and a `.py` class cannot
    /// have `double` fields — an element or a field has to be able to hold either type.
    /// So this is the setting the native compiler reads to choose a representation.
    pub strict_float: bool,

    /// Whether the basedpython "fluid specializations" feature is disabled.
    ///
    /// When disabled, inferred generic specializations are not widened flow-sensitively by
    /// later uses of a binding; each binding keeps its creation-time specialization.
    pub disable_fluid_specializations: bool,

    /// Whether to infer sound (non-gradual) types wherever a precise type is available, rather
    /// than falling back to a gradual type because an annotation is missing.
    ///
    /// This deliberately breaks the gradual guarantee. With it enabled: an unannotated parameter
    /// with a default is declared with the default's promoted type (`def f(a=1)` declares `int`),
    /// an unannotated method inherits the signature of the base method it overrides, a bare
    /// `ClassVar` declares its inferred type instead of `Unknown | <inferred>`, and an empty
    /// collection literal has element type `Never` rather than `Unknown`.
    pub sound_types: bool,

    /// Whether a function with no annotations is given the signature its body determines.
    ///
    /// Each unannotated parameter opens an anonymous type parameter — a `some` hole — bounded by
    /// everything the function requires of it, and a missing return type is the union of what the
    /// body returns. When this is disabled, an unannotated parameter accepts anything and an
    /// unannotated function returns `Unknown`, as the gradual guarantee requires.
    pub infer_unannotated_signatures: bool,

    /// Whether a private attribute leaves an inferred type parameter bivariant.
    ///
    /// A private (single-underscore or name-mangled) member is invisible to external observers,
    /// so it cannot be used to distinguish two specializations of its class and therefore
    /// constrains variance not at all. When this is disabled, private attributes are instead
    /// treated as immutable-but-readable, which constrains the type parameter to covariance.
    pub bivariant_private_attributes: bool,

    /// Whether a type variable that a call leaves unsolved is solved to `Never`.
    ///
    /// `Never` is the precise answer for a type variable that no argument mentions: no value ever
    /// reaches that position. When this is disabled, such a type variable falls back to the
    /// gradual `Unknown`. A PEP 696 default always takes priority over either.
    ///
    /// Only an occurrence that is an output is solved this way; an invariant or contravariant
    /// occurrence stays gradual, since `Never` there would say nothing can be written or passed.
    pub precise_unsolved_typevars: bool,

    /// Classes whose values do not count as a distinct member of an overlapping condition.
    ///
    /// Entries are qualified class names (`decimal.Decimal`); a class in `builtins` may also be
    /// spelled bare (`int`), and `None` stands for the type of `None`.
    pub overlapping_condition_exempt_types: Box<[Box<str>]>,

    /// Whether an instance with no `__bool__` and no `__len__` counts as always truthy when
    /// looking for an overlapping condition.
    ///
    /// Such an instance is only *ambiguously* truthy — a subclass may define `__bool__` — so by
    /// default it is a falsy member of `if not x` just as `None` is. Enabling this assumes the
    /// class means what it looks like it means, which drops the reports for the very common
    /// `if not x` over an optional instance.
    pub overlapping_condition_assume_truthy_instances: bool,

    /// Classes never reported as rendering through `object.__repr__`.
    ///
    /// Entries are qualified class names (`decimal.Decimal`); a class in `builtins` may also be
    /// spelled bare (`int`). A class deriving from one of these is exempt too.
    pub implicit_object_repr_exempt_types: Box<[Box<str>]>,

    /// Classes whose stub is taken at its word when looking for a value rendered through
    /// `object.__repr__`.
    ///
    /// A stub normally settles nothing, because it omits `__str__` and `__repr__` whether or not
    /// the runtime class has them. For a class listed here the omission counts as real, the same
    /// way it would for a class written in source.
    pub implicit_object_repr_report_types: Box<[Box<str>]>,
}

/// the stdlib classes whose default rendering says nothing about the value, and
/// which therefore default to being reported
///
/// membership is decided by one question asked of a real interpreter — does
/// `repr(v)` contain `hex(id(v))` — not by whether the stub declares a
/// `__repr__`. the two come apart in both directions: `generator` defines one
/// and it is still an address, while `threading.Thread` and `itertools.count`
/// declare nothing and print perfectly well. a class that ends up here is still
/// only reported when nothing in its hierarchy supplies a rendering, so a
/// subclass that writes a `__repr__` is quiet
///
/// only the stdlib is listed. an extension class from somewhere else cannot be
/// judged from its stub, and is left alone rather than guessed at
const OPAQUE_REPR_CLASSES: &[&str] = &[
    // `<function f at 0x…>`
    "types.FunctionType",
    // the one entry the address question does not settle: `<class 'C'>` carries
    // no address, but it identifies the class rather than a value, and printing
    // a class where an instance was meant is the same mistake
    "builtins.type",
    // the lazy sequences, which are printed instead of consumed
    "types.GeneratorType",
    "types.AsyncGeneratorType",
    "types.CoroutineType",
    "builtins.map",
    "builtins.filter",
    "builtins.zip",
    "builtins.enumerate",
    "builtins.reversed",
    "itertools.chain",
    "itertools.islice",
    "itertools.cycle",
    "itertools.accumulate",
    "itertools.groupby",
    "itertools.product",
    "itertools.permutations",
    // `_thread` renames its lock across versions, so both spellings are listed
    "_thread.LockType",
    "_thread.lock",
    "_thread.RLock",
    "threading.Event",
    "threading.Semaphore",
    "contextlib.ExitStack",
    "contextlib.AsyncExitStack",
    "builtins.memoryview",
    "builtins.property",
    "builtins.staticmethod",
    "builtins.classmethod",
];

impl AnalysisSettings {
    /// the settings a project starts from under `preset`, before its own `analysis` table
    pub fn from_preset(preset: TypeCheckingPreset) -> Self {
        let basedpython = preset.is_strict();

        Self {
            strict_generic_narrowing: false,
            strict_equality_semantics: false,
            respect_type_ignore_comments: true,
            allowed_unresolved_imports: ModuleGlobSet::empty(),
            replace_imports_with_any: ModuleGlobSet::empty(),
            dependency_groups: None,
            shipped_modules: None,
            exported_dependencies: None,
            strict_float: false,
            disable_fluid_specializations: !basedpython,
            sound_types: basedpython,
            infer_unannotated_signatures: basedpython,
            bivariant_private_attributes: basedpython,
            precise_unsolved_typevars: basedpython,
            overlapping_condition_exempt_types: Box::default(),
            overlapping_condition_assume_truthy_instances: false,
            implicit_object_repr_exempt_types: Box::default(),
            implicit_object_repr_report_types: OPAQUE_REPR_CLASSES
                .iter()
                .map(|name| Box::from(*name))
                .collect(),
        }
    }
}

impl Default for AnalysisSettings {
    fn default() -> Self {
        Self::from_preset(TypeCheckingPreset::default())
    }
}

/// Returns all attribute assignments (and their method scope IDs) with a symbol name matching
/// the one given for a specific class body scope.
///
/// Only call this when doing type inference on the same file as `class_body_scope`, otherwise it
/// introduces a direct dependency on that file's AST.
pub(crate) fn attribute_assignments<'db, 's>(
    db: &'db dyn Db,
    class_body_scope: ScopeId<'db>,
    name: &'s str,
) -> impl Iterator<Item = (BindingWithConstraintsIterator<'db, 'db>, FileScopeId)> + use<'s, 'db> {
    let index = semantic_index(db, class_body_scope.program_file(db));

    attribute_scopes(db, class_body_scope).filter_map(|function_scope_id| {
        let place_table = index.place_table(function_scope_id);
        let member = place_table.member_id_by_instance_attribute_name(name)?;
        let use_def = index.use_def_map(function_scope_id);
        Some((use_def.reachable_member_bindings(member), function_scope_id))
    })
}

/// Returns all attribute declarations (and their method scope IDs) with a symbol name matching
/// the one given for a specific class body scope.
///
/// Only call this when doing type inference on the same file as `class_body_scope`, otherwise it
/// introduces a direct dependency on that file's AST.
pub(crate) fn attribute_declarations<'db, 's>(
    db: &'db dyn Db,
    class_body_scope: ScopeId<'db>,
    name: &'s str,
) -> impl Iterator<Item = (DeclarationsIterator<'db, 'db>, FileScopeId)> + use<'s, 'db> {
    let index = semantic_index(db, class_body_scope.program_file(db));

    attribute_scopes(db, class_body_scope).filter_map(|function_scope_id| {
        let place_table = index.place_table(function_scope_id);
        let member = place_table.member_id_by_instance_attribute_name(name)?;
        let use_def = index.use_def_map(function_scope_id);
        Some((
            use_def.reachable_member_declarations(member),
            function_scope_id,
        ))
    })
}

/// Get the module-level docstring for the given file.
pub(crate) fn module_docstring(db: &dyn Db, file: PythonFile<'_>) -> Option<String> {
    let module = parsed_module(db, file).load(db);
    docstring_from_body(module.suite())
        .map(|docstring_expr| docstring_expr.value.to_str().to_owned())
}

pub fn check_file_unwrap(db: &dyn Db, file: ProgramFile<'_>) -> Vec<Diagnostic> {
    check_file(db, file)
        .map(<[ruff_db::diagnostic::Diagnostic]>::into_vec)
        .unwrap_or_else(|error| vec![error])
}

pub fn check_file(db: &dyn Db, file: ProgramFile<'_>) -> Result<Box<[Diagnostic]>, Diagnostic> {
    check_file_with(db, file, Vec::new())
}

/// [`check_file`], with lint diagnostics worked out elsewhere folded in.
///
/// `external` are diagnostics about `file` that this crate cannot compute — the
/// django route checks read the project's whole url tree, which is not something
/// a pass over one file can see. They arrive with the file's suppression comments
/// *not* applied, so that applying them happens here alongside the type checker's
/// own: one `ty: ignore` then silences either kind of diagnostic, and counts as
/// used either way.
pub fn check_file_with(
    db: &dyn Db,
    file: ProgramFile<'_>,
    external: Vec<Diagnostic>,
) -> Result<Box<[Diagnostic]>, Diagnostic> {
    with_display_for_file(db, file.file(db), || check_file_inner(db, file, external))
}

/// Run `body` with the type display `file` is written in: basedpython surface
/// syntax (`(1, 2)` rather than `tuple[Literal[1], Literal[2]]`) for a `.by`
/// file, the standard typing-spec spelling otherwise.
///
/// Anything that renders a type *for* a file — a diagnostic, a hover, an inlay
/// hint — should go through this, or it will spell types in a syntax the file
/// cannot be written in.
pub fn with_display_for_file<R>(db: &dyn Db, file: File, body: impl FnOnce() -> R) -> R {
    if file.source_type(db).is_basedpython() {
        crate::types::display::with_basedpython_display(body)
    } else {
        body()
    }
}

fn check_file_inner(
    db: &dyn Db,
    file: ProgramFile<'_>,
    external: Vec<Diagnostic>,
) -> Result<Box<[Diagnostic]>, Diagnostic> {
    let source_file = file.file(db);
    let mut diagnostics: Vec<Diagnostic> = Vec::new();

    // Abort checking if there are IO errors.
    let source = source_text(db, source_file);

    if let Some(read_error) = source.read_error() {
        return Err(IOErrorDiagnostic {
            file: source_file,
            error: read_error.clone(),
        }
        .to_diagnostic());
    }

    let parsed = parsed_module(db, file.python_file(db));

    let parsed_ref = parsed.load(db);
    diagnostics.extend(
        parsed_ref
            .errors()
            .iter()
            .map(|error| Diagnostic::invalid_syntax(source_file, &error.error, error)),
    );

    diagnostics.extend(parsed_ref.unsupported_syntax_errors().iter().map(|error| {
        let mut error = Diagnostic::invalid_syntax(source_file, error, error);
        add_inferred_python_version_hint_to_diagnostic(
            db,
            source_file,
            &mut error,
            "parsing syntax",
        );
        error
    }));

    diagnostics.extend(check_types_with(db, file, external));

    diagnostics.sort_unstable_by(|a, b| a.rendering_sort_key(db).cmp(&b.rendering_sort_key(db)));

    Ok(diagnostics.into_boxed_slice())
}

#[derive(Debug, Clone, get_size2::GetSize)]
pub struct IOErrorDiagnostic {
    file: File,
    error: SourceTextError,
}

impl IOErrorDiagnostic {
    fn to_diagnostic(&self) -> Diagnostic {
        let mut diag = Diagnostic::new(DiagnosticId::Io, Severity::Error, &self.error);
        diag.annotate(Annotation::primary(Span::from(self.file)));
        diag
    }
}

/// Many type-inference queries union together results from previous iterations to
/// ensure convergence. However, the first couple iterations are often prone to get
/// values that will soon converge, but where unioning in the early value causes an
/// unrecoverable loss of precision. This constant controls how many iterations
/// are considered likely to produce "tainted" results that should be discarded.
const TAINTED_CYCLES: u32 = 3;
