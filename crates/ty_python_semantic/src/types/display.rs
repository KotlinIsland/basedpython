//! Display implementations for types.

use crate::ProgramEnvironment;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::fmt::{self, Display, Formatter, Write};
use std::rc::Rc;

use ruff_db::files::FilePath;
use ruff_db::parsed::parsed_module;
use ruff_db::source::{line_index, source_text};
use ruff_python_ast as ast;
use ruff_python_ast::str::{Quote, TripleQuotes};
use ruff_python_literal::escape::AsciiEscape;
use ruff_source_file::LineColumn;
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};
use rustc_hash::{FxHashMap, FxHashSet};

use ty_module_resolver::{KnownModule, Module, file_to_module};

use crate::Db;
use crate::place::{DefinedPlace, Place, builtins_symbol, global_symbol};
use crate::types::callable::CallableTypeKind;
use crate::types::class::{ClassLiteral, ClassType, DynamicNamedTupleAnchor, GenericAlias};
use crate::types::constraints::ConstraintSetBuilder;
use crate::types::function::{FunctionType, OverloadLiteral};
use crate::types::generics::{GenericContext, Specialization};
use crate::types::protocol_class::{InlineProtocolMemberForm, ProtocolInterface};
use crate::types::signatures::{
    CallableSignature, Parameter, Parameters, ParametersKind, Signature,
};
use crate::types::tuple::{TupleSpec, VariableSegment};
use crate::types::typevar::BoundTypeVarIdentity;
use crate::types::visitor::TypeVisitor;
use crate::types::{
    CallableType, DeferredOperation, DeferredType, DynamicType, IntersectionType,
    KnownBoundMethodType, KnownClass, KnownInstanceType, KnownUnion, LiteralValueType,
    LiteralValueTypeKind, MaterializationKind, ParamSpecAttrKind, PropertyInstanceClass,
    PropertyInstanceType, Protocol, SpecialFormType, StringLiteralType, SubclassOfInner,
    SubclassOfType, Type, TypeAliasType, TypeGuardLike, TypedDictType, TypingModule,
    UnionType, WrapperDescriptorKind, template::TemplatePart, visitor,
};
use ty_python_core::ProgramFile;
use ty_python_core::definition::Definition;
use ty_python_core::scope::{FileScopeId, ScopeKind};
use ty_python_core::semantic_index;

/// A named item that can be either a class or a type alias.
///
/// This wrapper allows tracking both classes and type aliases together for
/// disambiguation, since a class and type alias with the same name in different
/// modules need to be distinguished in error messages.
#[derive(Clone, Copy, Debug)]
enum NamedItem<'db> {
    Class(ClassLiteral<'db>),
    TypeAlias(TypeAliasType<'db>),
}

impl<'db> NamedItem<'db> {
    fn is_same_item(self, db: &'db dyn Db, other: Self) -> bool {
        match (self, other) {
            (NamedItem::Class(left), NamedItem::Class(right)) => left == right,
            (NamedItem::TypeAlias(left), NamedItem::TypeAlias(right)) => {
                // Specializations of the same alias share a display name.
                left.definition(db) == right.definition(db)
            }
            _ => false,
        }
    }

    fn name(self, db: &'db dyn Db) -> &'db str {
        match self {
            NamedItem::Class(class) => class.name(db),
            NamedItem::TypeAlias(type_alias) => type_alias.name(db),
        }
    }

    fn qualified_name_components(self, db: &'db dyn Db) -> Vec<String> {
        match self {
            NamedItem::Class(class) => class.qualified_name(db).components_excluding_self(),
            NamedItem::TypeAlias(type_alias) => {
                type_alias.qualified_name(db).components_excluding_self()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum SignatureNameDisplay {
    #[default]
    Auto,
    Force,
    Disallow,
}

impl SignatureNameDisplay {
    const fn should_display(self, multiline: bool) -> bool {
        match self {
            Self::Auto => multiline,
            Self::Force => true,
            Self::Disallow => false,
        }
    }

    const fn allows_type_parameters(self) -> bool {
        !matches!(self, Self::Disallow)
    }
}

/// Controls whether numeric-tower unions use annotation spelling or expose their exact members.
///
/// basedpython only ever expands them. A reader is told what a type *is*, and the
/// promotion is surfaced as an inlay hint on the modules that enable python's float
/// semantics rather than hidden inside a type that reads as something narrower.
#[derive(Debug, Clone, Copy, Default)]
enum NumericTowerDisplay {
    /// Display every exact member, such as `int | float`.
    #[default]
    Expanded,
}

/// Settings for displaying types and signatures
#[derive(Debug, Clone, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent rendering options"
)]
pub struct DisplaySettings<'db> {
    /// Whether rendering can be multiline
    multiline: bool,
    /// Whether callable signatures should include their definition name.
    signature_name_display: SignatureNameDisplay,
    /// Class names that should be displayed fully qualified
    /// (e.g., `module.ClassName` instead of just `ClassName`)
    qualified: Rc<FxHashMap<&'db str, QualificationLevel>>,
    /// Type alias names that should be displayed fully qualified
    /// (e.g., `A.Alias` instead of just `Alias`)
    qualified_type_aliases: Rc<FxHashMap<&'db str, QualificationLevel>>,
    /// Whether long unions and literals are displayed in full
    preserve_full_unions: bool,
    /// How numeric-tower unions should be displayed.
    numeric_tower_display: NumericTowerDisplay,
    /// Scopes that are currently active in the display context (e.g. function scopes
    /// whose type parameters are currently being displayed).
    /// Used to suppress redundant `@{scope}` suffixes for type variables.
    active_scopes: Rc<FxHashSet<Definition<'db>>>,
    /// Function types that are currently being displayed.
    /// Used to prevent infinite recursion when displaying self-referential function types.
    visited_function_types: Rc<FxHashSet<FunctionType<'db>>>,
    /// Whether to hide the return type of the outermost signature.
    /// Return types of nested callable types inside parameters are still shown.
    hide_return_type: bool,
    /// basedpython: whether the caller has already written the `def <name>` this signature
    /// belongs to, as the bound-method display does. Such a signature is a *declaration*, so it
    /// leaves out a `None` return the way the source may.
    pub name_already_written: bool,
    /// basedpython: whether a specialization names the type parameter each of
    /// its arguments fills (`A[Key=str, Value=int]`), the way a keyword
    /// subscript writes it. Only ever set for `.by` output — python's subscript
    /// grammar has no keyword form.
    pub name_type_arguments: bool,
    /// basedpython: whether a symbolic arithmetic operation is shown as the type it
    /// reduces to (`int`) rather than as the expression it stands for (`I + 1`). Only
    /// ever set by the transpiler: an expression reads better everywhere a human sees
    /// it, but emitting one as python would evaluate `_I + 1` on a `TypeVar` object at
    /// import time.
    pub reduce_symbolic_operations: bool,
}

impl<'db> DisplaySettings<'db> {
    /// basedpython: name the type parameter each type argument fills.
    #[must_use]
    pub fn with_named_type_arguments(&self) -> Self {
        Self {
            name_type_arguments: true,
            ..self.clone()
        }
    }

    /// basedpython: show a symbolic arithmetic operation as the type it reduces to, for
    /// output that has to be valid python.
    #[must_use]
    pub fn with_reduced_symbolic_operations(&self) -> Self {
        Self {
            reduce_symbolic_operations: true,
            ..self.clone()
        }
    }

    #[must_use]
    fn multiline(&self) -> Self {
        Self {
            multiline: true,
            ..self.clone()
        }
    }

    #[must_use]
    fn singleline(&self) -> Self {
        Self {
            multiline: false,
            ..self.clone()
        }
    }

    /// Begin displaying the signature of `function`, or `None` when doing so would recurse.
    ///
    /// A function's signature can name the function itself: an inferred return type of
    /// `self.f` makes `f` return a callable over `f`. Rendering that nests forever, so every
    /// site that writes a signature belonging to a `FunctionType` must go through here — the
    /// exhausted result is a truncated `(...)`. The depth limit catches the case where the
    /// nested function is an equal-but-distinct value, as it is once a signature has been
    /// rebound to a receiver.
    #[must_use]
    fn enter_function(&self, function: FunctionType<'db>) -> Option<Self> {
        const MAX_FUNCTION_TYPE_DISPLAY_DEPTH: usize = 4;
        if self.visited_function_types.contains(&function)
            || self.visited_function_types.len() >= MAX_FUNCTION_TYPE_DISPLAY_DEPTH
        {
            return None;
        }
        let mut visited = (*self.visited_function_types).clone();
        visited.insert(function);
        Some(Self {
            visited_function_types: Rc::new(visited),
            ..self.clone()
        })
    }

    #[must_use]
    fn preserve_long_unions(self) -> Self {
        Self {
            preserve_full_unions: true,
            ..self
        }
    }

    /// Expands numeric-tower unions so explanations can refer to their individual members.
    ///
    /// For example, a relation error that discusses the `int` member of a `float` annotation
    /// displays the union as `int | float*` instead of hiding that member behind `float`.
    #[must_use]
    pub(crate) fn expand_numeric_tower_unions(&self) -> Self {
        Self {
            numeric_tower_display: NumericTowerDisplay::Expanded,
            ..self.clone()
        }
    }

    #[must_use]
    pub(crate) fn disallow_signature_name(&self) -> Self {
        Self {
            signature_name_display: SignatureNameDisplay::Disallow,
            ..self.clone()
        }
    }

    #[must_use]
    fn force_signature_name(&self) -> Self {
        Self {
            signature_name_display: SignatureNameDisplay::Force,
            ..self.clone()
        }
    }

    #[must_use]
    fn hide_return_type(&self) -> Self {
        Self {
            hide_return_type: true,
            ..self.clone()
        }
    }

    /// basedpython: this signature's `def <name>` has already been written, so it is a
    /// declaration rather than a callable type.
    #[must_use]
    fn name_already_written(&self) -> Self {
        Self {
            name_already_written: true,
            ..self.clone()
        }
    }

    #[must_use]
    fn with_active_scopes(&self, scopes: impl IntoIterator<Item = Definition<'db>>) -> Self {
        let mut active_scopes = (*self.active_scopes).clone();
        active_scopes.extend(scopes);
        Self {
            active_scopes: Rc::new(active_scopes),
            ..self.clone()
        }
    }

    #[must_use]
    fn with_generic_context(
        &self,
        db: &'db dyn Db,
        generic_context: Option<&GenericContext<'db>>,
    ) -> Self {
        if let Some(generic_context) = generic_context {
            self.with_active_scopes(
                generic_context
                    .variables(db)
                    .filter_map(|bound| bound.binding_context(db).definition()),
            )
        } else {
            self.clone()
        }
    }

    #[must_use]
    pub fn from_possibly_ambiguous_types<I, T>(
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        types: I,
    ) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<Type<'db>>,
    {
        fn build_display_settings<'db>(
            collector: &AmbiguousNameCollector<'_, 'db>,
        ) -> DisplaySettings<'db> {
            // Both classes and type aliases use the same qualification map since
            // a class and type alias with the same name need to be disambiguated.
            let qualification_map = Rc::new(collector.qualification_map());
            DisplaySettings {
                qualified: Rc::clone(&qualification_map),
                qualified_type_aliases: qualification_map,
                ..DisplaySettings::default()
            }
        }

        let collector = AmbiguousNameCollector {
            env,
            visited_types: RefCell::default(),
            names: RefCell::default(),
        };

        for ty in types {
            collector.visit_type(db, ty.into());
        }

        build_display_settings(&collector)
    }
}

/// Details about a type's formatting
///
/// The `targets` and `details` are 1:1 (you can `zip` them)
pub struct TypeDisplayDetails<'db> {
    /// The fully formatted type
    pub label: String,
    /// Ranges in the label
    pub targets: Vec<TextRange>,
    /// Metadata for each range
    pub details: Vec<TypeDetail<'db>>,
    /// Whether the label is valid Python syntax
    pub is_valid_syntax: bool,
}

/// Abstraction over "are we doing normal formatting, or tracking ranges with metadata?"
enum TypeWriter<'a, 'b, 'db> {
    Formatter(&'a mut Formatter<'b>),
    Details(TypeDetailsWriter<'db>),
}
/// Writer that builds a string with range tracking
struct TypeDetailsWriter<'db> {
    label: String,
    targets: Vec<TextRange>,
    details: Vec<TypeDetail<'db>>,
    is_valid_syntax: bool,
}

impl<'db> TypeDetailsWriter<'db> {
    fn new() -> Self {
        Self {
            label: String::new(),
            targets: Vec::new(),
            details: Vec::new(),
            is_valid_syntax: true,
        }
    }

    /// Produce type info
    fn finish_type_details(self) -> TypeDisplayDetails<'db> {
        TypeDisplayDetails {
            label: self.label,
            targets: self.targets,
            details: self.details,
            is_valid_syntax: self.is_valid_syntax,
        }
    }

    /// Produce function signature info
    fn finish_signature_details(self) -> SignatureDisplayDetails {
        // We use SignatureStart and SignatureEnd to delimit nested function signatures inside
        // this function signature. We only care about the parameters of the outermost function
        // which should introduce it's own SignatureStart and SignatureEnd
        let mut parameter_ranges = Vec::new();
        let mut parameter_names = Vec::new();
        let mut parameter_nesting = 0;
        for (target, detail) in self.targets.into_iter().zip(self.details) {
            match detail {
                TypeDetail::SignatureStart => parameter_nesting += 1,
                TypeDetail::SignatureEnd => parameter_nesting -= 1,
                TypeDetail::Parameter(parameter) => {
                    if parameter_nesting <= 1 {
                        // We found parameters at the top-level, record them
                        parameter_names.push(parameter);
                        parameter_ranges.push(target);
                    }
                }
                TypeDetail::Type(_) => { /* don't care */ }
            }
        }

        SignatureDisplayDetails {
            label: self.label,
            parameter_names,
            parameter_ranges,
        }
    }
}

impl<'a, 'b, 'db> TypeWriter<'a, 'b, 'db> {
    /// Indicate the given detail is about to start being written to this Writer
    ///
    /// This creates a scoped guard that when Dropped will record the given detail
    /// as spanning from when it was introduced to when it was dropped.
    fn with_detail<'c>(&'c mut self, detail: TypeDetail<'db>) -> TypeDetailGuard<'a, 'b, 'c, 'db> {
        let start = match self {
            TypeWriter::Formatter(_) => None,
            TypeWriter::Details(details) => Some(details.label.text_len()),
        };
        TypeDetailGuard {
            start,
            inner: self,
            payload: Some(detail),
        }
    }

    /// Convenience for `with_detail(TypeDetail::Type(ty))`
    fn with_type<'c>(&'c mut self, ty: Type<'db>) -> TypeDetailGuard<'a, 'b, 'c, 'db> {
        self.with_detail(TypeDetail::Type(ty))
    }

    fn set_invalid_type_annotation(&mut self) {
        match self {
            TypeWriter::Formatter(_) => {}
            TypeWriter::Details(details) => details.is_valid_syntax = false,
        }
    }

    fn join<'c>(&'c mut self, separator: &'static str) -> Join<'a, 'b, 'c, 'db> {
        Join {
            fmt: self,
            separator,
            result: Ok(()),
            seen_first: false,
        }
    }
}

impl Write for TypeWriter<'_, '_, '_> {
    fn write_str(&mut self, val: &str) -> fmt::Result {
        match self {
            TypeWriter::Formatter(formatter) => formatter.write_str(val),
            TypeWriter::Details(formatter) => formatter.write_str(val),
        }
    }
}
impl Write for TypeDetailsWriter<'_> {
    fn write_str(&mut self, val: &str) -> fmt::Result {
        self.label.write_str(val)
    }
}

trait FmtDetailed<'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result;
}

struct Join<'a, 'b, 'c, 'db> {
    fmt: &'c mut TypeWriter<'a, 'b, 'db>,
    separator: &'static str,
    result: fmt::Result,
    seen_first: bool,
}

impl<'db> Join<'_, '_, '_, 'db> {
    fn entry(&mut self, item: &dyn FmtDetailed<'db>) -> &mut Self {
        if self.seen_first {
            self.result = self
                .result
                .and_then(|()| self.fmt.write_str(self.separator));
        } else {
            self.seen_first = true;
        }
        self.result = self.result.and_then(|()| item.fmt_detailed(self.fmt));
        self
    }

    fn entries<I, F>(&mut self, items: I) -> &mut Self
    where
        I: IntoIterator<Item = F>,
        F: FmtDetailed<'db>,
    {
        for item in items {
            self.entry(&item);
        }
        self
    }

    fn finish(&mut self) -> fmt::Result {
        self.result
    }
}

pub enum TypeDetail<'db> {
    /// Dummy item to indicate a function signature's parameters have started
    SignatureStart,
    /// Dummy item to indicate a function signature's parameters have ended
    SignatureEnd,
    /// A function signature's parameter
    Parameter(String),
    /// A type
    Type(Type<'db>),
}

/// Look on my Works, ye Mighty, and despair!
///
/// It's quite important that we avoid conflating any of these lifetimes, or else the
/// borrowchecker will throw a ton of confusing errors about things not living long
/// enough. If you get those kinds of errors, it's probably because you introduced
/// something like `&'db self`, which, while convenient, and sometimes works, is imprecise.
struct TypeDetailGuard<'a, 'b, 'c, 'db> {
    inner: &'c mut TypeWriter<'a, 'b, 'db>,
    start: Option<TextSize>,
    payload: Option<TypeDetail<'db>>,
}

impl Drop for TypeDetailGuard<'_, '_, '_, '_> {
    fn drop(&mut self) {
        // The fallibility here is primarily retrieving `TypeWriter::Details`
        // everything else is ideally-never-fails pedantry (yay for pedantry!)
        if let TypeWriter::Details(details) = &mut self.inner
            && let Some(start) = self.start
            && let Some(payload) = self.payload.take()
        {
            let target = TextRange::new(start, details.label.text_len());
            details.targets.push(target);
            details.details.push(payload);
        }
    }
}

impl<'a, 'b, 'db> std::ops::Deref for TypeDetailGuard<'a, 'b, '_, 'db> {
    type Target = TypeWriter<'a, 'b, 'db>;
    fn deref(&self) -> &Self::Target {
        self.inner
    }
}
impl std::ops::DerefMut for TypeDetailGuard<'_, '_, '_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QualificationLevel {
    ModuleName,
    FileAndLineNumber,
}

impl QualificationLevel {
    const fn from_ambiguity_state(state: &AmbiguityState) -> Option<Self> {
        match state {
            AmbiguityState::Unambiguous(_) => None,
            AmbiguityState::RequiresFullyQualifiedName { .. } => Some(Self::ModuleName),
            AmbiguityState::RequiresFileAndLineNumber => Some(Self::FileAndLineNumber),
        }
    }
}

struct AmbiguousNameCollector<'a, 'db> {
    env: &'a ProgramEnvironment<'db>,
    visited_types: RefCell<FxHashSet<Type<'db>>>,
    names: RefCell<FxHashMap<&'db str, AmbiguityState<'db>>>,
}

impl<'db> AmbiguousNameCollector<'_, 'db> {
    /// Records an item for ambiguity tracking.
    ///
    /// This updates the ambiguity state for items with the same name:
    /// - First occurrence: `Unambiguous`
    /// - Different qualified paths: `RequiresFullyQualifiedName`
    /// - Same qualified paths: `RequiresFileAndLineNumber`
    fn record(&self, db: &'db dyn Db, item: NamedItem<'db>) {
        match self.names.borrow_mut().entry(item.name(db)) {
            Entry::Vacant(entry) => {
                entry.insert(AmbiguityState::Unambiguous(item));
            }
            Entry::Occupied(mut entry) => {
                let value = entry.get_mut();
                match value {
                    AmbiguityState::Unambiguous(existing) => {
                        if !existing.is_same_item(db, item) {
                            let qualified_name_components = item.qualified_name_components(db);
                            if existing.qualified_name_components(db) == qualified_name_components {
                                *value = AmbiguityState::RequiresFileAndLineNumber;
                            } else {
                                *value = AmbiguityState::RequiresFullyQualifiedName {
                                    item,
                                    qualified_name_components,
                                };
                            }
                        }
                    }
                    AmbiguityState::RequiresFullyQualifiedName {
                        item: existing,
                        qualified_name_components,
                    } => {
                        if !existing.is_same_item(db, item) {
                            let new_components = item.qualified_name_components(db);
                            if *qualified_name_components == new_components {
                                *value = AmbiguityState::RequiresFileAndLineNumber;
                            }
                        }
                    }
                    AmbiguityState::RequiresFileAndLineNumber => {}
                }
            }
        }
    }

    fn record_class(&self, db: &'db dyn Db, class: ClassLiteral<'db>) {
        self.record(db, NamedItem::Class(class));
    }

    fn record_type_alias(&self, db: &'db dyn Db, type_alias: TypeAliasType<'db>) {
        self.record(db, NamedItem::TypeAlias(type_alias));
    }

    /// Returns the qualification level map for all names.
    ///
    /// When there's any ambiguity for a name (including conflicts between a class
    /// and a type alias), the name is included so that items with that name get qualified.
    fn qualification_map(&self) -> FxHashMap<&'db str, QualificationLevel> {
        self.names
            .borrow()
            .iter()
            .filter_map(|(name, ambiguity)| {
                Some((*name, QualificationLevel::from_ambiguity_state(ambiguity)?))
            })
            .collect()
    }
}

/// Whether or not an item can be unambiguously identified by its *unqualified* name
/// given the other types that are present in the same context.
#[derive(Debug, Clone)]
enum AmbiguityState<'db> {
    /// The item can be displayed unambiguously using its unqualified name.
    Unambiguous(NamedItem<'db>),
    /// The item must be displayed using its fully qualified name to avoid ambiguity.
    RequiresFullyQualifiedName {
        item: NamedItem<'db>,
        qualified_name_components: Vec<String>,
    },
    /// Even the item's fully qualified name is not sufficient;
    /// we must also include the file and line number.
    RequiresFileAndLineNumber,
}

impl<'db> TypeVisitor<'db> for AmbiguousNameCollector<'_, 'db> {
    fn program_environment(&self) -> &ProgramEnvironment<'db> {
        self.env
    }

    fn should_visit_lazy_type_attributes(&self) -> bool {
        false
    }

    fn visit_type(&self, db: &'db dyn Db, ty: Type<'db>) {
        match ty {
            Type::ClassLiteral(class) => self.record_class(db, class),
            Type::LiteralValue(literal) => {
                if let LiteralValueTypeKind::Enum(literal) = literal.kind() {
                    self.record_class(db, literal.enum_class(db));
                }
            }
            Type::GenericAlias(alias) => {
                self.record_class(db, ClassLiteral::Static(alias.origin(db)));
            }
            Type::TypeAlias(type_alias) => self.record_type_alias(db, type_alias),
            // Visit the class (as if it were a nominal-instance type)
            // rather than the protocol members, if it is a class-based protocol.
            // (For the purposes of displaying the type, we'll use the class name.)
            Type::ProtocolInstance(protocol) if let Some(class) = protocol.class_origin(db) => {
                return self.visit_type(db, Type::from(class));
            }
            // no need to recurse into TypeVar bounds/constraints
            Type::TypeVar(_) => return,
            _ => {}
        }

        if let visitor::TypeKind::NonAtomic(t) = visitor::TypeKind::from(ty) {
            if !self.visited_types.borrow_mut().insert(ty) {
                // If we have already seen this type, we can skip it.
                return;
            }
            visitor::walk_non_atomic_type(db, t, self);
        }
    }
}

impl<'db> Type<'db> {
    pub fn display<'env>(
        self,
        db: &'db dyn Db,
        env: &'env ProgramEnvironment<'db>,
    ) -> DisplayType<'env, 'db> {
        DisplayType {
            ty: self,
            settings: DisplaySettings::from_possibly_ambiguous_types(db, env, [self]),
            db,
            env,
        }
    }

    pub fn display_with<'env>(
        self,
        db: &'db dyn Db,
        env: &'env ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplayType<'env, 'db> {
        DisplayType {
            ty: self,
            db,
            env,
            settings,
        }
    }

    /// The value a literal type stands for, written the way a source writes it —
    /// `1`, `"a"`, `True` — rather than as the type spelling `Literal[1]`.
    ///
    /// Returns `None` for anything that is not one concrete literal value: a
    /// `LiteralString` or a template literal, which each stand for a *set* of
    /// strings, and an enum member, whose value is the type it was declared
    /// with rather than a value of its own.
    pub fn display_value<'env>(
        self,
        db: &'db dyn Db,
        env: &'env ProgramEnvironment<'db>,
    ) -> Option<impl fmt::Display + use<'env, 'db>> {
        match self.as_literal_value_kind()? {
            LiteralValueTypeKind::Int(_)
            | LiteralValueTypeKind::Bool(_)
            | LiteralValueTypeKind::String(_)
            | LiteralValueTypeKind::Bytes(_)
            | LiteralValueTypeKind::Float(_)
            | LiteralValueTypeKind::Complex(_) => {
                Some(self.representation(db, env, DisplaySettings::default()))
            }
            LiteralValueTypeKind::LiteralString
            | LiteralValueTypeKind::Template(_)
            | LiteralValueTypeKind::Enum(_) => None,
        }
    }

    /// basedpython: this value written the way a *parameter default* writes it — `1`, `"a"`,
    /// `None`, `...`.
    ///
    /// A method's defaults are part of what it declares, so an override that leaves one out takes
    /// the base's. Carrying it that far means writing it into the override's own signature, which
    /// only a value with a spelling can be: everything [`Type::display_value`] spells, plus the
    /// two singletons a signature writes that are not literal types.
    ///
    /// An expression that is not one of those — a list display, a call, a name — is left behind.
    /// basedpython re-evaluates such a default on every call, so what it stands for is the
    /// expression rather than any one value, and there is nothing to carry.
    pub fn display_default_value(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> Option<String> {
        if self.is_none(db) {
            return Some("None".to_string());
        }
        if matches!(
            self,
            Type::NominalInstance(instance)
                if instance.has_known_class(db, KnownClass::EllipsisType)
        ) {
            return Some("...".to_string());
        }
        // an overflowing literal such as `1e400` *is* a float value, but not one python has a
        // literal for: it is spelled `inf`, and writing that back is a name nothing binds. a
        // not-a-number is the same, and so is either as part of a complex
        match self.as_literal_value_kind()? {
            LiteralValueTypeKind::Float(value) if !value.as_f64().is_finite() => return None,
            LiteralValueTypeKind::Complex(value)
                if !value.re(db).is_finite() || !value.im(db).is_finite() =>
            {
                return None;
            }
            _ => {}
        }
        Some(self.display_value(db, env)?.to_string())
    }

    fn representation<'env>(
        self,
        db: &'db dyn Db,
        env: &'env ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplayRepresentation<'env, 'db> {
        DisplayRepresentation {
            db,
            env,
            ty: self,
            settings,
        }
    }
}

pub struct DisplayType<'env, 'db> {
    ty: Type<'db>,
    db: &'db dyn Db,
    env: &'env ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> DisplayType<'_, 'db> {
    /// Allows this type display to span multiple lines while preserving inferred qualification.
    #[must_use]
    pub fn multiline(self) -> Self {
        Self {
            settings: self.settings.multiline(),
            ..self
        }
    }

    #[must_use]
    pub(crate) fn preserve_long_unions(self) -> Self {
        Self {
            settings: self.settings.preserve_long_unions(),
            ..self
        }
    }

    pub fn to_string_parts(&self) -> TypeDisplayDetails<'db> {
        let mut f = TypeWriter::Details(TypeDetailsWriter::new());
        self.fmt_detailed(&mut f).unwrap();

        match f {
            TypeWriter::Details(details) => details.finish_type_details(),
            TypeWriter::Formatter(_) => unreachable!("Expected Details variant"),
        }
    }
}

impl<'db> FmtDetailed<'db> for DisplayType<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let db = self.db;
        let representation = self.ty.representation(db, self.env, self.settings.clone());
        match self.ty.as_literal_value_kind() {
            Some(
                LiteralValueTypeKind::Int(_)
                | LiteralValueTypeKind::Bool(_)
                | LiteralValueTypeKind::String(_)
                | LiteralValueTypeKind::Bytes(_)
                | LiteralValueTypeKind::Enum(_),
            ) if basedpython_display_enabled() => {
                // basedpython surface syntax — literals render as their bare
                // repr without the `Literal[...]` wrapper since the wrapper
                // adds noise without disambiguating
                representation.fmt_detailed(f)
            }
            Some(
                LiteralValueTypeKind::Int(_)
                | LiteralValueTypeKind::Bool(_)
                | LiteralValueTypeKind::String(_)
                | LiteralValueTypeKind::Bytes(_)
                | LiteralValueTypeKind::Enum(_),
            ) => {
                f.with_type(Type::SpecialForm(SpecialFormType::Literal))
                    .write_str("Literal")?;
                f.write_char('[')?;
                representation.fmt_detailed(f)?;
                f.write_str("]")
            }
            _ => representation.fmt_detailed(f),
        }
    }
}

impl Display for DisplayType<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

impl fmt::Debug for DisplayType<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, f)
    }
}

thread_local! {
    /// thread-local switch enabling basedpython-style type display.
    /// turned on while emitting diagnostics for `.by` files; off otherwise
    /// so the standard typing-spec display remains the default for `.py`
    static BASEDPYTHON_DISPLAY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn basedpython_display_enabled() -> bool {
    BASEDPYTHON_DISPLAY.with(std::cell::Cell::get)
}

/// Run `f` with basedpython-style type display enabled. Used by
/// diagnostic emission for `.by` files
pub(crate) fn with_basedpython_display<R>(f: impl FnOnce() -> R) -> R {
    BASEDPYTHON_DISPLAY.with(|cell| {
        let prev = cell.replace(true);
        let result = f();
        cell.set(prev);
        result
    })
}

/// Format a file location suffix for disambiguation (e.g., " @ path:line:column")
fn fmt_file_location<'db>(
    db: &'db dyn Db,
    file: ruff_db::files::File,
    offset: TextSize,
    f: &mut TypeWriter<'_, '_, 'db>,
) -> fmt::Result {
    let path = file.path(db);
    let path = match path {
        FilePath::System(path) => Cow::Owned(FilePath::from(
            path.strip_prefix(db.system().current_directory())
                .unwrap_or(path)
                .to_path_buf(),
        )),
        FilePath::Vendored(_) | FilePath::SystemVirtual(_) => Cow::Borrowed(path),
    };
    let line_index = line_index(db, file);
    let LineColumn { line, column } = line_index.line_column(offset, &source_text(db, file));
    f.set_invalid_type_annotation();
    write!(f, " @ {path}:{line}:{column}")
}

/// basedpython: a type rendered as python source that resolves inside a particular file.
///
/// ty's ordinary display names a class by its bare name whatever the reading file binds
/// that name to, which is right for a diagnostic and wrong for source the transpiler
/// emits: `datetime` there is the *module*, and a class the file never imported is not
/// bound at all. See [`Type::source_spelling_in`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSpelling {
    /// the type expression
    pub text: String,
    /// modules the spelling needs, each as an `import <module>` target
    pub modules: Vec<String>,
}

/// Collects every class a type names, so each can be spelled for a particular file.
struct ClassCollector<'a, 'db> {
    env: &'a ProgramEnvironment<'db>,
    visited_types: RefCell<FxHashSet<Type<'db>>>,
    classes: RefCell<Vec<ClassLiteral<'db>>>,
}

impl<'db> ClassCollector<'_, 'db> {
    fn record(&self, class: ClassLiteral<'db>) {
        self.classes.borrow_mut().push(class);
    }
}

impl<'db> TypeVisitor<'db> for ClassCollector<'_, 'db> {
    fn program_environment(&self) -> &ProgramEnvironment<'db> {
        self.env
    }

    fn should_visit_lazy_type_attributes(&self) -> bool {
        false
    }

    fn visit_type(&self, db: &'db dyn Db, ty: Type<'db>) {
        match ty {
            Type::ClassLiteral(class) => self.record(class),
            Type::LiteralValue(literal) => {
                if let LiteralValueTypeKind::Enum(literal) = literal.kind() {
                    self.record(literal.enum_class(db));
                }
            }
            Type::GenericAlias(alias) => self.record(ClassLiteral::Static(alias.origin(db))),
            Type::ProtocolInstance(protocol) if let Some(class) = protocol.class_origin(db) => {
                return self.visit_type(db, Type::from(class));
            }
            Type::TypeVar(_) => return,
            _ => {}
        }

        if let visitor::TypeKind::NonAtomic(t) = visitor::TypeKind::from(ty) {
            if !self.visited_types.borrow_mut().insert(ty) {
                return;
            }
            visitor::walk_non_atomic_type(db, t, self);
        }
    }
}

impl<'db> Type<'db> {
    /// basedpython: spell this type as a python type expression that resolves inside `file`,
    /// along with the modules that spelling needs imported.
    ///
    /// A class `file` already binds under its own name — one it defines, one it imported, a
    /// builtin — is written bare, so an ordinary annotation comes out exactly as ty displays
    /// it. Any other class is written module-qualified (`decimal.Decimal`) and its module
    /// reported, because the bare name would be unbound there, or bound to something else.
    /// `None` when a class has no spelling at all: one local to a function, whose qualified
    /// name is not a dotted path.
    #[must_use]
    pub fn source_spelling_in(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        file: ProgramFile<'db>,
        settings: DisplaySettings<'db>,
    ) -> Option<SourceSpelling> {
        let collector = ClassCollector {
            env,
            visited_types: RefCell::default(),
            classes: RefCell::default(),
        };
        collector.visit_type(db, self);

        let classes = collector.classes.borrow();
        let mut qualified = FxHashMap::default();
        for class in classes.iter().copied() {
            let name = class.name(db);
            if !file_binds_class(db, env, file, name, class) {
                qualified.insert(&**name, QualificationLevel::ModuleName);
            }
        }
        // qualification is keyed by name, so one class forces every same-named
        // class in the type to be written qualified too — each of those needs
        // its module just as much
        let mut modules = std::collections::BTreeSet::new();
        for class in classes.iter().copied() {
            if !qualified.contains_key(&**class.name(db)) {
                continue;
            }
            let module = importable_module_of(db, env, class)?;
            match file_binds_module(db, file, &module) {
                // the file already reaches the module under that name, so the
                // qualified spelling resolves as it stands
                ModuleBinding::Same => {}
                ModuleBinding::Absent => {
                    modules.insert(module);
                }
                // importing it would rebind a name the file uses for something
                // else, so this class cannot be spelled here at all
                ModuleBinding::Other => return None,
            }
        }

        let settings = DisplaySettings {
            qualified: Rc::new(qualified),
            ..settings
        };
        Some(SourceSpelling {
            text: self.display_with(db, env, settings).to_string(),
            modules: modules.into_iter().collect(),
        })
    }
}

/// Whether `name` reaches `class` from `file`'s module scope — a class the file defines or
/// imports, or a builtin, all of which a bare name spells correctly there.
fn file_binds_class<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    name: &str,
    class: ClassLiteral<'db>,
) -> bool {
    let binds = |place: Place<'db>| matches!(place, Place::Defined(defined) if defined.ty == Type::ClassLiteral(class));
    binds(global_symbol(db, file, name).place) || binds(builtins_symbol(db, env, name).place)
}

/// What a file's module scope already binds the leading name of a qualified spelling to.
enum ModuleBinding {
    /// the module itself — the spelling resolves with no import
    Same,
    /// nothing — the import can be added
    Absent,
    /// something else entirely — an import would collide with it
    Other,
}

/// How `file` binds the name a spelling of `module` reads first (`a` in `a.b.C`).
fn file_binds_module<'db>(db: &'db dyn Db, file: ProgramFile<'db>, module: &str) -> ModuleBinding {
    let root = module.split('.').next().unwrap_or(module);
    match global_symbol(db, file, root).place {
        Place::Defined(defined) => match defined.ty {
            Type::ModuleLiteral(literal) if literal.module(db).name(db) == module => {
                ModuleBinding::Same
            }
            _ => ModuleBinding::Other,
        },
        Place::Undefined => ModuleBinding::Absent,
    }
}

/// The module to import so that `class`'s qualified name resolves, or `None` when the class
/// has no importable spelling — it is local to a function, so its qualified name carries a
/// `<locals of …>` component that is not a python path.
fn importable_module_of<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    class: ClassLiteral<'db>,
) -> Option<String> {
    let file = ProgramFile::new(db, class.file(db), env.program(db));
    let module = file_to_module(db, file.resolver_file(db))?;
    let components = class.qualified_name(db).components_excluding_self();
    if !components
        .iter()
        .all(|component| ruff_python_stdlib::identifiers::is_identifier(component))
    {
        return None;
    }
    Some(public_module_name(db, &module).to_owned())
}

/// The name a module should be written under, which is its own name unless the module is a
/// private implementation detail that another module is the public face of.
///
/// The only such module is `_collections_abc`, where the `collections.abc` ABCs (`Mapping`,
/// `Iterator`, …) are defined; `collections.abc` is nothing but `from _collections_abc import *`,
/// and `typing` re-exports the same classes again. Naming the private module in a diagnostic
/// would point at a spelling nobody writes.
pub(super) fn public_module_name<'db>(db: &'db dyn Db, module: &Module<'db>) -> &'db str {
    if module.known(db) == Some(KnownModule::CollectionsAbcInternal) {
        KnownModule::CollectionsAbc.as_str()
    } else {
        module.name(db).as_str()
    }
}

/// Returns the qualified name components for a scope, excluding the item itself.
///
/// This is the shared logic used by both [`QualifiedClassName`](super::class::QualifiedClassName)
/// and [`QualifiedTypeAliasName`](super::type_alias::QualifiedTypeAliasName) to compute the path
/// components (module, enclosing classes, functions) leading to an item.
///
/// # Returns
/// A vector of path components in order (e.g., `["module", "OuterClass", "InnerClass"]`)
pub(super) fn qualified_name_components_from_scope(
    db: &dyn Db,
    file: ProgramFile<'_>,
    file_scope_id: FileScopeId,
    skip_count: usize,
) -> Vec<String> {
    let module_ast = parsed_module(db, file.python_file(db)).load(db);
    let index = semantic_index(db, file);

    let mut name_parts = vec![];

    for (_, ancestor_scope) in index.ancestor_scopes(file_scope_id).skip(skip_count) {
        let node = ancestor_scope.node();

        match ancestor_scope.kind() {
            ScopeKind::Class => {
                if let Some(class_def) = node.as_class() {
                    name_parts.push(class_def.node(&module_ast).name.as_str().to_string());
                }
            }
            ScopeKind::Function => {
                if let Some(function_def) = node.as_function() {
                    name_parts.push(format!(
                        "<locals of function '{}'>",
                        function_def.node(&module_ast).name.as_str()
                    ));
                }
            }
            _ => {}
        }
    }

    if let Some(module) = file_to_module(db, file.resolver_file(db)) {
        name_parts.push(public_module_name(db, &module).to_string());
    }

    name_parts.reverse();
    name_parts
}

impl<'db> ClassLiteral<'db> {
    pub(crate) fn display_with<'env>(
        self,
        db: &'db dyn Db,
        env: &'env ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> ClassDisplay<'env, 'db> {
        ClassDisplay {
            db,
            env,
            class: self,
            settings,
        }
    }
}

pub(crate) struct ClassDisplay<'env, 'db> {
    db: &'db dyn Db,
    env: &'env ProgramEnvironment<'db>,
    class: ClassLiteral<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> FmtDetailed<'db> for ClassDisplay<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let env = self.env;
        // basedpython anonymous named tuples render as their surface syntax
        // `(name: T, ...)` rather than the synthesized `_AnonNamedTuple_<hash>`
        // class name. Positional fields use the synthetic `arg<i>` name and
        // are rendered without a label
        if let ClassLiteral::DynamicNamedTuple(nt) = self.class
            && nt.name(self.db).as_str().starts_with("_AnonNamedTuple_")
        {
            let spec = match nt.anchor(self.db) {
                DynamicNamedTupleAnchor::CollectionsDefinition { spec, .. }
                | DynamicNamedTupleAnchor::ScopeOffset { spec, .. } => Some(*spec),
                DynamicNamedTupleAnchor::TypingDefinition(_) => None,
            };
            if let Some(spec) = spec {
                let fields = spec.fields(self.db);
                let ty = Type::ClassLiteral(self.class);
                let mut f = f.with_type(ty);
                f.write_char('(')?;
                for (i, field) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    let synthetic_pos = field.name.as_str() == format!("arg{i}");
                    if !synthetic_pos {
                        write!(f, "{}: ", field.name)?;
                    }
                    field
                        .ty
                        .display_with(self.db, env, self.settings.clone())
                        .fmt_detailed(&mut f)?;
                }
                f.write_char(')')?;
                return Ok(());
            }
        }

        let qualification_level = self.settings.qualified.get(&**self.class.name(self.db));

        let ty = Type::ClassLiteral(self.class);
        if qualification_level.is_some() {
            let qualified_name = self.class.qualified_name(self.db);
            write!(f.with_type(ty), "{qualified_name}")?;
        } else {
            write!(f.with_type(ty), "{}", self.class.name(self.db))?;
        }

        if qualification_level == Some(&QualificationLevel::FileAndLineNumber) {
            let file = self.class.file(self.db);
            let offset = self.class.header_range(self.db).start();
            fmt_file_location(self.db, file, offset, f)?;
        }
        Ok(())
    }
}

impl Display for ClassDisplay<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

impl<'db> TypeAliasType<'db> {
    fn display_with(
        self,
        db: &'db dyn Db,
        settings: DisplaySettings<'db>,
    ) -> TypeAliasDisplay<'db> {
        TypeAliasDisplay {
            db,
            type_alias: self,
            settings,
        }
    }

    /// Returns a source-style display of this type alias's declaration.
    pub fn display_declaration<'env>(
        self,
        db: &'db dyn Db,
        env: &'env ProgramEnvironment<'db>,
    ) -> impl Display + 'env {
        let value_ty = self.raw_value_type(db);
        DisplayTypeAliasDeclaration {
            db,
            env,
            type_alias: self,
            value_ty,
            settings: DisplaySettings::from_possibly_ambiguous_types(
                db,
                env,
                [Type::TypeAlias(self), value_ty],
            ),
        }
    }
}

struct TypeAliasDisplay<'db> {
    db: &'db dyn Db,
    type_alias: TypeAliasType<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> FmtDetailed<'db> for TypeAliasDisplay<'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let qualification_level = self
            .settings
            .qualified_type_aliases
            .get(self.type_alias.name(self.db));

        let ty = Type::TypeAlias(self.type_alias);
        if qualification_level.is_some() {
            let qualified_name = self.type_alias.qualified_name(self.db);
            write!(f.with_type(ty), "{qualified_name}")?;
        } else {
            write!(f.with_type(ty), "{}", self.type_alias.name(self.db))?;
        }

        if qualification_level == Some(&QualificationLevel::FileAndLineNumber) {
            let definition = self.type_alias.definition(self.db);
            let file = definition.file(self.db);
            let offset = definition
                .focus_range(
                    self.db,
                    &parsed_module(self.db, definition.python_file(self.db)).load(self.db),
                )
                .range()
                .start();
            fmt_file_location(self.db, file, offset, f)?;
        }
        Ok(())
    }
}

impl Display for TypeAliasDisplay<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

/// A source-style display of a type alias declaration.
struct DisplayTypeAliasDeclaration<'env, 'db> {
    db: &'db dyn Db,
    env: &'env ProgramEnvironment<'db>,
    type_alias: TypeAliasType<'db>,
    value_ty: Type<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> FmtDetailed<'db> for DisplayTypeAliasDeclaration<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let db = self.db;
        let generic_context = self.type_alias.generic_context(db);
        let settings = self
            .settings
            .with_generic_context(db, generic_context.as_ref());

        f.write_str("type ")?;
        self.type_alias
            .display_with(db, settings.clone())
            .fmt_detailed(f)?;
        if let Some(generic_context) = generic_context {
            generic_context.display(db).fmt_detailed(f)?;
        }
        f.write_str(" = ")?;
        self.value_ty
            .display_with(db, self.env, settings)
            .fmt_detailed(f)
    }
}

impl Display for DisplayTypeAliasDeclaration<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

/// Helper for displaying `TypeGuardLike` types `TypeIs` and `TypeGuard`.
fn fmt_type_guard_like<'db, T: TypeGuardLike<'db>>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    guard: T,
    settings: &DisplaySettings<'db>,
    f: &mut TypeWriter<'_, '_, 'db>,
) -> fmt::Result {
    f.with_type(Type::SpecialForm(T::special_form()))
        .write_str(T::FORM_NAME)?;
    f.write_char('[')?;
    guard
        .type_argument(db)
        .display_with(db, env, settings.singleline())
        .fmt_detailed(f)?;
    if let Some(name) = guard.place_name(db) {
        f.set_invalid_type_annotation();
        f.write_str(" @ ")?;
        f.write_str(&name)?;
    }
    f.write_str("]")
}

/// basedpython: how tightly a symbolic operation binds, so that a nested operand is
/// parenthesised exactly when the expression would otherwise read as a different one.
/// Anything that is not such an operation is a leaf and never needs parentheses; so are
/// the postfix operations — an attribute access and a call — which bind tighter than any
/// operator and so are never parenthesised themselves.
fn deferred_binding_power(db: &dyn Db, ty: Type<'_>) -> u8 {
    let Type::Deferred(deferred) = ty else {
        return u8::MAX;
    };
    match deferred.operation(db) {
        DeferredOperation::Binary(ast::Operator::Add | ast::Operator::Sub) => 1,
        DeferredOperation::Binary(ast::Operator::Mult) => 2,
        DeferredOperation::Unary(_) => 3,
        _ => u8::MAX,
    }
}

/// basedpython: the receiver and member name a symbolic call's callee was reached through,
/// when it was reached through one at all.
fn deferred_call_receiver<'db>(
    db: &'db dyn Db,
    callee: Type<'db>,
) -> Option<(Type<'db>, &'db str)> {
    match callee {
        Type::BoundMethod(method) => Some((method.self_instance(db), method.function(db).name(db))),
        Type::Deferred(deferred) => match (deferred.operation(db), deferred.operands(db)) {
            (DeferredOperation::Attribute(name), [receiver]) => Some((*receiver, name.as_str())),
            _ => None,
        },
        _ => None,
    }
}

/// basedpython: write a symbolic operation back out as the expression it stands for, e.g.
/// `I@succ + 1` or `s@starts.startswith("foo")`. `minimum_binding_power` is what the
/// surrounding operator requires of this position; a weaker-binding operation there is
/// parenthesised.
fn fmt_deferred_operation<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    deferred: DeferredType<'db>,
    settings: &DisplaySettings<'db>,
    minimum_binding_power: u8,
    f: &mut TypeWriter<'_, '_, 'db>,
) -> fmt::Result {
    let binding_power = deferred_binding_power(db, Type::Deferred(deferred));
    let parenthesised = binding_power < minimum_binding_power;
    if parenthesised {
        f.write_char('(')?;
    }

    let operand = |f: &mut TypeWriter<'_, '_, 'db>, ty: Type<'db>, minimum: u8| match ty {
        Type::Deferred(nested) if nested.is_checked(db) => {
            fmt_deferred_operation(db, env, nested, settings, minimum, f)
        }
        _ => ty.display_with(db, env, settings.clone()).fmt_detailed(f),
    };

    // a receiver has to bind tighter than every operator, so an arithmetic one is
    // parenthesised while a nested access or call is not
    let receiver = u8::MAX;

    match (deferred.operation(db), deferred.operands(db)) {
        (DeferredOperation::Binary(op), [left, right]) => {
            operand(f, *left, binding_power)?;
            f.write_char(' ')?;
            f.write_str(op.as_str())?;
            f.write_char(' ')?;
            // `a - (b - c)` is not `a - b - c`, so the right operand of a left-associative
            // operator has to bind one step tighter to go unparenthesised
            operand(f, *right, binding_power + 1)?;
        }
        (DeferredOperation::Unary(op), [inner]) => {
            f.write_str(op.as_str())?;
            operand(f, *inner, binding_power)?;
        }
        // the callee carries the receiver it was bound to, which is what the annotation
        // was written against — `s.startswith` reads back as `s@f.startswith`. a class
        // member carries it as the bound method's instance, a structural one as the
        // attribute type's own receiver
        (DeferredOperation::Call, [callee, args @ ..])
            if let Some((callee_receiver, method_name)) = deferred_call_receiver(db, *callee) =>
        {
            operand(f, callee_receiver, receiver)?;
            f.write_char('.')?;
            f.write_str(method_name)?;
            f.write_char('(')?;
            for (index, arg) in args.iter().enumerate() {
                if index > 0 {
                    f.write_str(", ")?;
                }
                operand(f, *arg, 0)?;
            }
            f.write_char(')')?;
        }
        // `is_checked` admits no other shape; a deferral built with the wrong operand
        // count, or a call through a callee that names no receiver, is still better
        // shown reduced than not at all
        _ => Type::Deferred(deferred)
            .reduce_deferred(db, env)
            .display_with(db, env, settings.clone())
            .fmt_detailed(f)?,
    }

    if parenthesised {
        f.write_char(')')?;
    }
    Ok(())
}

/// Writes the string representation of a type, which is the value displayed either as
/// `Literal[<repr>]` or `Literal[<repr1>, <repr2>]` for literal types or as `<repr>` for
/// non literals
struct DisplayRepresentation<'env, 'db> {
    ty: Type<'db>,
    db: &'db dyn Db,
    env: &'env ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

fn property_display_name<'db>(db: &'db dyn Db, property: PropertyInstanceType<'db>) -> &'db str {
    match property.instance_class(db) {
        PropertyInstanceClass::Builtin => "property",
        PropertyInstanceClass::Enum => "enum.property",
        PropertyInstanceClass::Subclass(class) => class.name(db),
    }
}

impl Display for DisplayRepresentation<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

impl<'db> FmtDetailed<'db> for DisplayRepresentation<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let env = self.env;
        let db = self.db;
        match self.ty {
            Type::Dynamic(dynamic) => {
                if dynamic.is_todo() {
                    f.set_invalid_type_annotation();
                }
                write!(f.with_type(self.ty), "{dynamic}")
            }
            Type::Divergent(_) => f.with_type(self.ty).write_str("Divergent"),
            Type::Never => f.with_type(self.ty).write_str("Never"),
            Type::NominalInstance(instance) => {
                let class = instance.class(db, self.env);

                match (class, class.known(db)) {
                    (_, Some(KnownClass::NoneType)) => f.with_type(self.ty).write_str("None"),
                    (_, Some(KnownClass::NoDefaultType)) => {
                        f.with_type(self.ty).write_str("NoDefault")
                    }
                    (ClassType::Generic(alias), Some(KnownClass::Tuple)) => alias
                        .specialization(db)
                        .tuple(db)
                        .expect(
                            "Specialization::tuple() should always return `Some()` for \
                            `KnownClass::Tuple`",
                        )
                        .display_with(db, self.env, self.settings.clone())
                        .fmt_detailed(f),
                    (ClassType::NonGeneric(class), _) => class
                        .display_with(db, env, self.settings.clone())
                        .fmt_detailed(f),
                    (ClassType::Generic(alias), _) => alias
                        .display_with(db, self.env, self.settings.clone())
                        .fmt_detailed(f),
                }
            }
            Type::ProtocolInstance(protocol) => match protocol.inner {
                Protocol::FromClass(class) => match *class {
                    ClassType::NonGeneric(class) => class
                        .display_with(db, env, self.settings.clone())
                        .fmt_detailed(f),
                    ClassType::Generic(alias) => alias
                        .display_with(db, self.env, self.settings.clone())
                        .fmt_detailed(f),
                },
                Protocol::Materialized(materialized) => {
                    let materialization_kind = protocol.display_materialization_kind(db, self.env);
                    if let Some(kind) = materialization_kind {
                        let (name, form) = match kind {
                            MaterializationKind::Top => ("Top", SpecialFormType::Top),
                            MaterializationKind::Bottom => ("Bottom", SpecialFormType::Bottom),
                        };
                        f.with_type(Type::SpecialForm(form)).write_str(name)?;
                        f.write_char('[')?;
                    }

                    match *materialized.origin(db) {
                        ClassType::NonGeneric(class) => class
                            .display_with(db, env, self.settings.clone())
                            .fmt_detailed(f),
                        ClassType::Generic(alias) => alias
                            .display_with(db, self.env, self.settings.clone())
                            .fmt_detailed(f),
                    }?;

                    if materialization_kind.is_some() {
                        f.write_char(']')?;
                    }
                    Ok(())
                }
                Protocol::Synthesized(synthetic) => {
                    // basedpython: a structural type *is* writable here — `protocol(...)` is
                    // the syntax that declares one — so it is spelled rather than described
                    if basedpython_display_enabled()
                        && let Some(inline) = DisplayInlineProtocol::new(
                            self.db,
                            env,
                            synthetic.interface(self.db),
                            self.settings.clone(),
                        )
                    {
                        return inline.fmt_detailed(f);
                    }
                    f.set_invalid_type_annotation();
                    f.write_char('<')?;
                    f.with_type(Type::SpecialForm(SpecialFormType::Protocol))
                        .write_str("Protocol")?;
                    let interface = synthetic.interface(self.db);
                    let member_list = interface.members(self.db);
                    let num_members = member_list.len();
                    if num_members == 0 && interface.pending_packs(self.db).is_empty() {
                        return f.write_str(" with no members>");
                    }
                    f.write_str(" with members ")?;
                    // basedpython: an unspecialized `protocol(**Kwargs)` pack contributes no
                    // members yet, so it is shown as written rather than omitted
                    let packs = interface.pending_packs(self.db);
                    for (i, member) in member_list.enumerate() {
                        let is_last = i + 1 == num_members && packs.is_empty();
                        write!(f, "'{}'", member.name())?;
                        if !is_last {
                            f.write_str(", ")?;
                        }
                    }
                    for (i, pack) in packs.iter().enumerate() {
                        write!(f, "**{}", pack.display(self.db, env))?;
                        if i + 1 != packs.len() {
                            f.write_str(", ")?;
                        }
                    }
                    f.write_char('>')
                }
            },
            Type::PropertyInstance(property)
                if let PropertyInstanceClass::Subclass(class) = property.instance_class(db) =>
            {
                Type::instance(db, self.env, class)
                    .display_with(db, self.env, self.settings.clone())
                    .fmt_detailed(f)
            }
            Type::PropertyInstance(property) => f
                .with_type(self.ty)
                .write_str(property_display_name(db, property)),
            Type::SlotDescriptor(_) => f
                .with_type(self.ty)
                .write_str(KnownClass::MemberDescriptorType.name(self.env.python_version(db))),
            Type::ModuleLiteral(module) => {
                f.set_invalid_type_annotation();
                f.write_char('<')?;
                f.with_type(KnownClass::ModuleType.to_class_literal(db, self.env))
                    .write_str("module")?;
                f.write_str(" '")?;
                f.with_type(self.ty).write_str(module.module(db).name(db))?;
                f.write_str("'>")
            }
            Type::ClassLiteral(class) => {
                f.set_invalid_type_annotation();
                let mut f = f.with_type(self.ty);
                f.write_str("<class '")?;
                class
                    .display_with(db, env, self.settings.clone())
                    .fmt_detailed(&mut f)?;
                f.write_str("'>")
            }
            Type::GenericAlias(generic) => {
                f.set_invalid_type_annotation();
                let mut f = f.with_type(self.ty);
                f.write_str("<class '")?;
                generic
                    .display_with(db, self.env, self.settings.clone())
                    .fmt_detailed(&mut f)?;
                f.write_str("'>")
            }
            Type::SubclassOf(subclass_of_ty) => match subclass_of_ty.subclass_of() {
                SubclassOfInner::Class(ClassType::NonGeneric(class)) => {
                    f.with_type(KnownClass::Type.to_class_literal(db, self.env))
                        .write_str("type")?;
                    f.write_char('[')?;
                    class
                        .display_with(db, env, self.settings.clone())
                        .fmt_detailed(f)?;
                    f.write_char(']')
                }
                SubclassOfInner::Class(ClassType::Generic(alias)) => {
                    f.with_type(KnownClass::Type.to_class_literal(db, self.env))
                        .write_str("type")?;
                    f.write_char('[')?;
                    alias
                        .display_with(db, self.env, self.settings.clone())
                        .fmt_detailed(f)?;
                    f.write_char(']')
                }
                SubclassOfInner::Dynamic(dynamic) => {
                    f.with_type(KnownClass::Type.to_class_literal(db, self.env))
                        .write_str("type")?;
                    f.write_char('[')?;
                    write!(f.with_type(Type::Dynamic(dynamic)), "{dynamic}")?;
                    f.write_char(']')
                }
                SubclassOfInner::Protocol(protocol) => {
                    f.with_type(KnownClass::Type.to_class_literal(db, self.env))
                        .write_str("type")?;
                    f.write_char('[')?;
                    Type::ProtocolInstance(protocol)
                        .display_with(db, self.env, self.settings.clone())
                        .fmt_detailed(f)?;
                    f.write_char(']')
                }
                SubclassOfInner::TypeVar(bound_typevar) => {
                    f.set_invalid_type_annotation();
                    f.with_type(KnownClass::Type.to_class_literal(db, self.env))
                        .write_str("type")?;
                    f.write_char('[')?;
                    write!(
                        f.with_type(Type::TypeVar(bound_typevar)),
                        "{}",
                        bound_typevar
                            .identity(db)
                            .display_with(db, self.settings.clone())
                    )?;
                    f.write_char(']')
                }
            },
            Type::SpecialForm(special_form) => {
                f.set_invalid_type_annotation();
                write!(f.with_type(self.ty), "<special-form '{special_form}'>")
            }
            Type::KnownInstance(known_instance) => known_instance
                .display_with(db, self.env, self.settings.clone())
                .fmt_detailed(f),
            Type::FunctionLiteral(function) => function
                .display_with(db, self.env, self.settings.clone())
                .fmt_detailed(f),
            Type::Callable(callable) => callable
                .display_with(db, self.env, self.settings.clone())
                .fmt_detailed(f),
            Type::BoundMethod(bound_method) => {
                let function = bound_method.function(self.db);
                let self_ty = bound_method.self_instance(self.db);
                let receiver_ty = bound_method.signature_receiver(self.db);

                let write_prefix = |f: &mut TypeWriter<'_, '_, 'db>| {
                    f.set_invalid_type_annotation();
                    f.write_str("bound method ")?;
                    DisplayMaybeParenthesizedType {
                        ty: self_ty,
                        db: self.db,
                        env: self.env,
                        settings: self.settings.singleline(),
                    }
                    .fmt_detailed(f)?;
                    if self_ty != receiver_ty {
                        f.write_str(" when ")?;
                        DisplayMaybeParenthesizedType {
                            ty: receiver_ty,
                            db: self.db,
                            env: self.env,
                            settings: self.settings.singleline(),
                        }
                        .fmt_detailed(f)?;
                    }
                    f.write_char('.')?;
                    f.with_type(self.ty).write_str(function.name(self.db))
                };

                let Some(settings) = self.settings.enter_function(function) else {
                    write_prefix(f)?;
                    return f.write_str("(...)");
                };

                let bound_signatures = bound_method.bound_signatures(self.db);

                match bound_signatures.overloads.as_slice() {
                    [signature] => {
                        let hide_unused_self =
                            signature.should_hide_self_from_display(db, self.env);
                        let type_parameters = DisplayOptionalGenericContext {
                            generic_context: signature.generic_context.as_ref(),
                            db: self.db,
                            hide_unused_self,
                        };
                        write_prefix(f)?;
                        type_parameters.fmt_detailed(f)?;
                        signature
                            .display_with(
                                self.db,
                                self.env,
                                settings.disallow_signature_name().name_already_written(),
                            )
                            .fmt_detailed(f)
                    }
                    signatures => {
                        // TODO: How to display overloads?
                        if !settings.multiline {
                            // TODO: This should ideally have a TypeDetail but we actually
                            // don't have a type for @overload (we just detect the decorator)
                            f.write_str("Overload")?;
                            f.write_char('[')?;
                        }
                        let separator = if settings.multiline { "\n" } else { ", " };
                        let mut join = f.join(separator);
                        for signature in signatures {
                            join.entry(&signature.display_with(
                                self.db,
                                self.env,
                                settings.clone(),
                            ));
                        }
                        join.finish()?;
                        if !settings.multiline {
                            f.write_str("]")?;
                        }
                        Ok(())
                    }
                }
            }
            Type::KnownBoundMethod(method_type) => {
                f.set_invalid_type_annotation();
                let (class_ty, member_name, cls_name, ty, ty_name) = match method_type {
                    KnownBoundMethodType::FunctionTypeDunderGet(function) => (
                        KnownClass::FunctionType.to_class_literal(db, self.env),
                        "__get__",
                        "function",
                        Type::FunctionLiteral(function),
                        Some(&**function.name(db)),
                    ),
                    KnownBoundMethodType::FunctionTypeDunderCall(function) => (
                        KnownClass::FunctionType.to_class_literal(db, self.env),
                        "__call__",
                        "function",
                        Type::FunctionLiteral(function),
                        Some(&**function.name(db)),
                    ),
                    KnownBoundMethodType::PropertyDunderGet(property) => (
                        property.instance_class(db).to_class_literal(db, self.env),
                        "__get__",
                        property_display_name(db, property),
                        Type::PropertyInstance(property),
                        property
                            .getter(db)
                            .and_then(Type::as_function_literal)
                            .map(|getter| &**getter.name(db)),
                    ),
                    KnownBoundMethodType::PropertyDunderSet(property) => (
                        property.instance_class(db).to_class_literal(db, self.env),
                        "__set__",
                        property_display_name(db, property),
                        Type::PropertyInstance(property),
                        property
                            .setter(db)
                            .and_then(Type::as_function_literal)
                            .map(|setter| &**setter.name(db)),
                    ),
                    KnownBoundMethodType::PropertyDunderDelete(property) => (
                        property.instance_class(db).to_class_literal(db, self.env),
                        "__delete__",
                        property_display_name(db, property),
                        Type::PropertyInstance(property),
                        property
                            .deleter(db)
                            .and_then(Type::as_function_literal)
                            .map(|deleter| &**deleter.name(db)),
                    ),
                    KnownBoundMethodType::StrStartswith(literal) => (
                        KnownClass::Property.to_class_literal(db, self.env),
                        "startswith",
                        "string",
                        Type::LiteralValue(LiteralValueType::promotable(
                            LiteralValueTypeKind::String(literal),
                        )),
                        Some(literal.value(db)),
                    ),
                    KnownBoundMethodType::ConstraintSetLowerBound => {
                        return f.write_str("bound method `ConstraintSet.lower_bound`");
                    }
                    KnownBoundMethodType::ConstraintSetUpperBound => {
                        return f.write_str("bound method `ConstraintSet.upper_bound`");
                    }
                    KnownBoundMethodType::ConstraintSetEquality => {
                        return f.write_str("bound method `ConstraintSet.equality`");
                    }
                    KnownBoundMethodType::ConstraintSetRange => {
                        return f.write_str("bound method `ConstraintSet.range`");
                    }
                    KnownBoundMethodType::ConstraintSetAlways => {
                        return f.write_str("bound method `ConstraintSet.always`");
                    }
                    KnownBoundMethodType::ConstraintSetNever => {
                        return f.write_str("bound method `ConstraintSet.never`");
                    }
                    KnownBoundMethodType::ConstraintSetImpliesSubtypeOf(_) => {
                        return f.write_str("bound method `ConstraintSet.implies_subtype_of`");
                    }
                    KnownBoundMethodType::ConstraintSetSatisfies(_) => {
                        return f.write_str("bound method `ConstraintSet.satisfies`");
                    }
                    KnownBoundMethodType::ConstraintSetExists(_) => {
                        return f.write_str("bound method `ConstraintSet.exists`");
                    }
                    KnownBoundMethodType::ConstraintSetForAll(_) => {
                        return f.write_str("bound method `ConstraintSet.for_all`");
                    }
                    KnownBoundMethodType::ConstraintSetSolutionsFor(_) => {
                        return f.write_str("bound method `ConstraintSet.solutions_for`");
                    }
                    KnownBoundMethodType::ConstraintSetSolutions(_) => {
                        return f.write_str("bound method `ConstraintSet.solutions`");
                    }
                    KnownBoundMethodType::ConstraintSetWithDetailedDisplay(_) => {
                        return f.write_str("bound method `ConstraintSet.with_detailed_display`");
                    }
                };

                f.write_char('<')?;
                f.with_type(KnownClass::MethodWrapperType.to_class_literal(db, self.env))
                    .write_str("method-wrapper")?;
                f.write_str(" '")?;
                if let Place::Defined(DefinedPlace { ty: member_ty, .. }) =
                    class_ty.member(db, self.env, member_name).place
                {
                    f.with_type(member_ty).write_str(member_name)?;
                } else {
                    f.write_str(member_name)?;
                }
                f.write_str("' of ")?;
                f.with_type(class_ty).write_str(cls_name)?;
                if let Some(name) = ty_name {
                    f.write_str(" '")?;
                    f.with_type(ty).write_str(name)?;
                    f.write_str("'>")
                } else {
                    f.write_str("' object>")
                }
            }
            Type::WrapperDescriptor(kind) => {
                f.set_invalid_type_annotation();
                let (method, object, cls) = match kind {
                    WrapperDescriptorKind::FunctionTypeDunderGet => {
                        ("__get__", "function", KnownClass::FunctionType)
                    }
                    WrapperDescriptorKind::PropertyDunderGet => {
                        ("__get__", "property", KnownClass::Property)
                    }
                    WrapperDescriptorKind::PropertyDunderSet => {
                        ("__set__", "property", KnownClass::Property)
                    }
                    WrapperDescriptorKind::PropertyDunderDelete => {
                        ("__delete__", "property", KnownClass::Property)
                    }
                };
                f.write_char('<')?;
                f.with_type(KnownClass::WrapperDescriptorType.to_class_literal(db, self.env))
                    .write_str("wrapper-descriptor")?;
                f.write_str(" '")?;
                f.write_str(method)?;
                f.write_str("' of '")?;
                f.with_type(cls.to_class_literal(db, self.env))
                    .write_str(object)?;
                f.write_str("' objects>")
            }
            Type::DataclassDecorator(_) => {
                f.set_invalid_type_annotation();
                f.write_str("<decorator produced by dataclass-like function>")
            }
            Type::DataclassTransformer(_) => {
                f.set_invalid_type_annotation();
                f.write_str("<decorator produced by typing.dataclass_transform>")
            }
            Type::Union(union) => union
                .display_with(db, self.env, self.settings.clone())
                .fmt_detailed(f),
            Type::Intersection(intersection) => intersection
                .display_with(db, self.env, self.settings.clone())
                .fmt_detailed(f),
            Type::EnumComplement(complement) => {
                if let Some(literals) =
                    complement.remaining_literal_types_for_display(db, self.env, LITERAL_POLICY.max)
                {
                    DisplayLiteralGroup {
                        literals,
                        db,
                        env: self.env,
                        settings: self.settings.clone(),
                    }
                    .fmt_detailed(f)
                } else {
                    complement
                        .to_intersection(db, self.env)
                        .display_with(db, self.env, self.settings.clone())
                        .fmt_detailed(f)
                }
            }
            Type::LiteralValue(literal) => match literal.kind() {
                LiteralValueTypeKind::Int(n) => write!(f.with_type(self.ty), "{n}"),
                LiteralValueTypeKind::Bool(boolean) => {
                    f.with_type(self.ty)
                        .write_str(if boolean { "True" } else { "False" })
                }
                LiteralValueTypeKind::String(string) => {
                    write!(f.with_type(self.ty), "{}", string.display(db))
                }
                // We used to return `str` as the type here because that feels generally more useful.
                // However, the inconsistency between the type shown in the inlay hint and its hover, and the
                // inconsistency to what's shown when hovering the backed inlay hint of a `LiteralString`
                // convinced us that we should change the type to `LiteralString`.
                LiteralValueTypeKind::LiteralString => f
                    .with_type(Type::SpecialForm(SpecialFormType::LiteralString))
                    .write_str("LiteralString"),
                LiteralValueTypeKind::Bytes(bytes) => {
                    let escape = AsciiEscape::with_preferred_quote(bytes.value(db), Quote::Double);

                    write!(
                        f.with_type(self.ty),
                        "{}",
                        escape.bytes_repr(TripleQuotes::No)
                    )
                }
                LiteralValueTypeKind::Enum(enum_literal) => {
                    enum_literal
                        .enum_class(db)
                        .display_with(db, env, self.settings.clone())
                        .fmt_detailed(f)?;
                    f.write_char('.')?;
                    write!(
                        f.with_type(Type::enum_literal(enum_literal)),
                        "{}",
                        enum_literal.name(db)
                    )
                }
                // basedpython: a template type is spelled the way it was written,
                // as an f-string whose holes are the hole types.
                //
                // each run of fixed text is written in one go, and the quotes and
                // braces carry no type at all: a `with_type` call is one
                // navigable region in an editor, so writing per character would
                // scatter the pattern into a region per character
                LiteralValueTypeKind::Template(template) => {
                    f.write_str("f\"")?;
                    for part in template.parts(self.db) {
                        match part {
                            TemplatePart::Text(text) => {
                                let mut escaped = String::with_capacity(text.len());
                                for character in text.chars() {
                                    match character {
                                        '{' => escaped.push_str("{{"),
                                        '}' => escaped.push_str("}}"),
                                        '"' => escaped.push_str("\\\""),
                                        '\\' => escaped.push_str("\\\\"),
                                        _ => {
                                            for escape in character.escape_debug() {
                                                escaped.push(escape);
                                            }
                                        }
                                    }
                                }
                                f.with_type(self.ty).write_str(&escaped)?;
                            }
                            TemplatePart::Hole(hole) => {
                                f.write_char('{')?;
                                hole.display_with(self.db, env, self.settings.clone())
                                    .fmt_detailed(f)?;
                                f.write_char('}')?;
                            }
                        }
                    }
                    f.write_char('"')
                }
                LiteralValueTypeKind::Float(v) => write!(f.with_type(self.ty), "{v}"),
                LiteralValueTypeKind::Complex(c) => {
                    let re = c.re(self.db);
                    let im = c.im(self.db);
                    if re == 0.0 {
                        write!(f.with_type(self.ty), "{im}j")
                    } else {
                        write!(f.with_type(self.ty), "({re}+{im}j)")
                    }
                }
            },
            Type::TypeVar(bound_typevar) => {
                f.set_invalid_type_annotation();
                write!(
                    f,
                    "{}",
                    bound_typevar
                        .identity(db)
                        .display_with(db, self.settings.clone())
                )
            }
            Type::AlwaysTruthy => f.with_type(self.ty).write_str("AlwaysTruthy"),
            Type::AlwaysFalsy => f.with_type(self.ty).write_str("AlwaysFalsy"),
            Type::BoundSuper(bound_super) => {
                f.set_invalid_type_annotation();
                f.write_str("<super: ")?;
                Type::from(bound_super.pivot_class(db))
                    .display_with(db, self.env, self.settings.singleline())
                    .fmt_detailed(f)?;
                f.write_str(", ")?;
                bound_super
                    .owner(db)
                    .owner_type()
                    .display_with(db, self.env, self.settings.singleline())
                    .fmt_detailed(f)?;
                f.write_str(">")
            }
            Type::TypeIs(type_is) => fmt_type_guard_like(db, self.env, type_is, &self.settings, f),
            Type::TypeGuard(type_guard) => {
                fmt_type_guard_like(db, self.env, type_guard, &self.settings, f)
            }
            Type::TypeForm(typeform) => {
                f.with_type(Type::SpecialForm(SpecialFormType::TypeForm))
                    .write_str("TypeForm")?;
                f.write_char('[')?;
                typeform
                    .type_argument(db)
                    .display_with(db, self.env, self.settings.clone())
                    .fmt_detailed(f)?;
                f.write_char(']')
            }
            Type::UnsafeUnion(unsafe_union) => {
                f.with_type(Type::SpecialForm(SpecialFormType::UnsafeUnion))
                    .write_str("UnsafeUnion")?;
                f.write_char('[')?;
                for (index, element) in unsafe_union.elements(self.db).iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    element
                        .display_with(self.db, env, self.settings.clone())
                        .fmt_detailed(f)?;
                }
                f.write_char(']')
            }
            Type::Restricted(restricted) => {
                f.write_str(restricted.modifier(self.db).keyword())?;
                f.write_char(' ')?;
                restricted
                    .type_argument(self.db)
                    .display_with(self.db, env, self.settings.clone())
                    .fmt_detailed(f)
            }
            Type::Overlapping(overlapping) => {
                f.with_type(Type::SpecialForm(SpecialFormType::Overlapping))
                    .write_str("Overlapping")?;
                f.write_char('[')?;
                overlapping
                    .type_argument(self.db)
                    .display_with(self.db, env, self.settings.clone())
                    .fmt_detailed(f)?;
                f.write_char(']')
            }
            // a checked operation is written back out as the expression it stands for: a
            // body checked against `I + 1` is rejected by naming a *different* value, and
            // the reduced form would report that as `int` against `int`
            Type::Deferred(deferred)
                if deferred.is_checked(self.db) && !self.settings.reduce_symbolic_operations =>
            {
                fmt_deferred_operation(self.db, env, deferred, &self.settings, 0, f)
            }
            // every other unspecialized operation displays as its reduced form (`T.a` shows
            // as the bound's `a`); once specialized it has folded to a concrete type and
            // this arm is not reached
            Type::Deferred(deferred) => deferred
                .reduced(self.db, env)
                .display_with(self.db, env, self.settings.clone())
                .fmt_detailed(f),
            Type::TypedDict(TypedDictType::Class(defining_class)) => {
                // basedpython: a dict-literal type reads back as the shape it was written as —
                // its generated class name is a hash and says nothing
                if let ClassType::NonGeneric(ClassLiteral::DynamicTypedDict(typeddict)) =
                    defining_class
                    && let Some((schema, packs)) = typeddict.synthesized_shape(self.db)
                {
                    f.write_char('{')?;
                    let mut wrote_any = false;
                    for (name, field) in schema {
                        if wrote_any {
                            f.write_str(", ")?;
                        }
                        write!(f, "\"{name}\": ")?;
                        field
                            .declared_ty
                            .display_with(self.db, env, self.settings.clone())
                            .fmt_detailed(f)?;
                        wrote_any = true;
                    }
                    for pack in packs {
                        if wrote_any {
                            f.write_str(", ")?;
                        }
                        f.write_str("**")?;
                        pack.display_with(self.db, env, self.settings.clone())
                            .fmt_detailed(f)?;
                        wrote_any = true;
                    }
                    return f.write_char('}');
                }
                match defining_class {
                    ClassType::NonGeneric(class) => class
                        .display_with(self.db, env, self.settings.clone())
                        .fmt_detailed(f),
                    ClassType::Generic(alias) => alias
                        .display_with(self.db, env, self.settings.clone())
                        .fmt_detailed(f),
                }
            }
            Type::TypedDict(typed_dict) if typed_dict.is_top(self.db) => f
                .with_type(Type::SpecialForm(SpecialFormType::TypedDict(
                    TypingModule::Typing,
                )))
                .write_str("TypedDict"),
            Type::TypedDict(TypedDictType::Synthesized(synthesized)) => {
                f.set_invalid_type_annotation();
                f.write_char('<')?;
                f.with_type(Type::SpecialForm(SpecialFormType::TypedDict(
                    TypingModule::Typing,
                )))
                .write_str("TypedDict")?;
                f.write_str(" with items ")?;
                let items = synthesized.items(db);
                for (i, name) in items.keys().enumerate() {
                    let is_last = i == items.len() - 1;
                    write!(f, "'{name}'")?;
                    if !is_last {
                        f.write_str(", ")?;
                    }
                }
                f.write_char('>')
            }
            Type::TypeAlias(alias) => {
                let materialization_kind = alias.materialization_kind(db);
                if let Some(kind) = materialization_kind {
                    let (name, form) = match kind {
                        MaterializationKind::Top => ("Top", SpecialFormType::Top),
                        MaterializationKind::Bottom => ("Bottom", SpecialFormType::Bottom),
                    };
                    f.with_type(Type::SpecialForm(form)).write_str(name)?;
                    f.write_char('[')?;
                }

                alias
                    .display_with(db, self.settings.clone())
                    .fmt_detailed(f)?;
                if let Some(specialization) = alias.specialization(db) {
                    specialization
                        .display_short(db, self.env, TupleSpecialization::No, self.settings.clone())
                        .fmt_detailed(f)?;
                }

                if materialization_kind.is_some() {
                    f.write_char(']')?;
                }
                Ok(())
            }
            Type::NewTypeInstance(newtype) => f.with_type(self.ty).write_str(newtype.name(db)),
        }
    }
}

impl<'db> BoundTypeVarIdentity<'db> {
    pub(crate) fn display(self, db: &'db dyn Db) -> impl Display {
        self.display_with(db, DisplaySettings::default())
    }

    fn display_with(self, db: &'db dyn Db, settings: DisplaySettings<'db>) -> impl Display {
        std::fmt::from_fn(move |f| {
            let paramspec_attr = self.paramspec_attr;
            // basedpython unpacks a parameter pack's two halves with stars — `*P` and `**P` —
            // rather than naming them as attributes of the type variable
            if basedpython_display_enabled()
                && let Some(attr) = paramspec_attr
            {
                f.write_str(match attr {
                    ParamSpecAttrKind::Args => "*",
                    ParamSpecAttrKind::Kwargs => "**",
                })?;
            }
            f.write_str(self.identity.name(db))?;
            let binding_context = self.binding_context;
            if let Some(binding_context_name) = binding_context.name(db)
                && let Some(definition) = binding_context.definition()
                && !settings.active_scopes.contains(&definition)
            {
                write!(f, "@{binding_context_name}")?;
            }
            if !basedpython_display_enabled()
                && let Some(attr) = paramspec_attr
            {
                write!(f, ".{attr}")?;
            }
            Ok(())
        })
    }
}

impl<'db> TupleSpec<'db> {
    fn display_with<'a>(
        &'a self,
        db: &'db dyn Db,
        env: &'a ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplayTuple<'a, 'db> {
        DisplayTuple {
            tuple: self,
            db,
            env,
            settings,
        }
    }
}

struct DisplayTuple<'a, 'db> {
    tuple: &'a TupleSpec<'db>,
    db: &'db dyn Db,
    env: &'a ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> FmtDetailed<'db> for DisplayTuple<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let db = self.db;
        let env = self.env;
        if basedpython_display_enabled() {
            return self.fmt_basedpython(f);
        }
        f.with_type(KnownClass::Tuple.to_class_literal(self.db, env))
            .write_str("tuple")?;
        f.write_char('[')?;
        match self.tuple {
            TupleSpec::Fixed(tuple) => {
                let elements = tuple.elements_slice();
                if elements.is_empty() {
                    f.write_str("()")?;
                } else {
                    elements
                        .display_with(db, self.env, self.settings.singleline())
                        .fmt_detailed(f)?;
                }
            }
            TupleSpec::Variable(tuple) => {
                if !tuple.prefix_elements().is_empty() {
                    tuple
                        .prefix_elements()
                        .display_with(db, self.env, self.settings.singleline())
                        .fmt_detailed(f)?;
                    f.write_str(", ")?;
                }
                match tuple.variable() {
                    VariableSegment::TypeVarTuple(typevar) => {
                        f.write_char('*')?;
                        Type::TypeVar(typevar)
                            .display_with(db, self.env, self.settings.singleline())
                            .fmt_detailed(f)?;
                    }
                    VariableSegment::Homogeneous(variable) => {
                        if !tuple.prefix_elements().is_empty()
                            || !tuple.suffix_elements().is_empty()
                        {
                            f.write_char('*')?;
                            // Might as well link the type again here too
                            f.with_type(KnownClass::Tuple.to_class_literal(db, self.env))
                                .write_str("tuple")?;
                            f.write_char('[')?;
                        }
                        variable
                            .display_with(db, self.env, self.settings.singleline())
                            .fmt_detailed(f)?;
                        f.write_str(", ...")?;
                        if !tuple.prefix_elements().is_empty()
                            || !tuple.suffix_elements().is_empty()
                        {
                            f.write_str("]")?;
                        }
                    }
                }
                if !tuple.suffix_elements().is_empty() {
                    f.write_str(", ")?;
                    tuple
                        .suffix_elements()
                        .display_with(db, self.env, self.settings.singleline())
                        .fmt_detailed(f)?;
                }
            }
        }
        f.write_str("]")
    }
}

impl<'db> DisplayTuple<'_, 'db> {
    /// basedpython surface syntax for tuple types:
    ///   tuple\[T1, T2\]                         → (T1, T2)
    ///   tuple\[T\]                              → (T,)
    ///   tuple\[T, ...\]                         → (*: T)
    ///   tuple\[prefix, *tuple\[V, ...\], suffix\] → (prefix, *: V, suffix)
    ///   tuple\[()\]                             → ()
    fn fmt_basedpython(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let env = self.env;
        f.with_type(KnownClass::Tuple.to_class_literal(self.db, env))
            .write_char('(')?;
        match self.tuple {
            TupleSpec::Fixed(tuple) => {
                let elements = tuple.elements_slice();
                if !elements.is_empty() {
                    elements
                        .display_with(self.db, env, self.settings.singleline())
                        .fmt_detailed(f)?;
                    if elements.len() == 1 {
                        f.write_char(',')?;
                    }
                }
            }
            TupleSpec::Variable(tuple) => {
                let mut first = true;
                for prefix in tuple.prefix_elements() {
                    if !first {
                        f.write_str(", ")?;
                    }
                    first = false;
                    prefix
                        .display_with(self.db, env, self.settings.singleline())
                        .fmt_detailed(f)?;
                }
                if !first {
                    f.write_str(", ")?;
                }
                f.write_str("*: ")?;
                match tuple.variable() {
                    VariableSegment::Homogeneous(variable) => variable
                        .display_with(self.db, env, self.settings.singleline())
                        .fmt_detailed(f)?,
                    VariableSegment::TypeVarTuple(typevar) => Type::TypeVar(typevar)
                        .display_with(self.db, env, self.settings.singleline())
                        .fmt_detailed(f)?,
                }
                for suffix in tuple.suffix_elements() {
                    f.write_str(", ")?;
                    suffix
                        .display_with(self.db, env, self.settings.singleline())
                        .fmt_detailed(f)?;
                }
            }
        }
        f.write_char(')')
    }
}

impl Display for DisplayTuple<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

impl<'db> OverloadLiteral<'db> {
    // Not currently used, but useful for debugging.
    #[expect(dead_code)]
    fn display<'env>(
        self,
        db: &'db dyn Db,
        env: &'env ProgramEnvironment<'db>,
    ) -> DisplayOverloadLiteral<'env, 'db> {
        Self::display_with(self, db, env, DisplaySettings::default())
    }

    fn display_with<'env>(
        self,
        db: &'db dyn Db,
        env: &'env ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplayOverloadLiteral<'env, 'db> {
        DisplayOverloadLiteral {
            literal: self,
            db,
            env,
            settings,
        }
    }
}

pub(crate) struct DisplayOverloadLiteral<'env, 'db> {
    literal: OverloadLiteral<'db>,
    db: &'db dyn Db,
    env: &'env ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> FmtDetailed<'db> for DisplayOverloadLiteral<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let env = self.env;
        let db = self.db;
        let signature = self.literal.signature(db);
        let hide_unused_self = signature.should_hide_self_from_display(db, self.env);
        let type_parameters = DisplayOptionalGenericContext {
            generic_context: signature.generic_context.as_ref(),
            db,
            hide_unused_self,
        };

        f.set_invalid_type_annotation();
        f.write_str("def ")?;
        write!(f, "{}", self.literal.name(db))?;
        type_parameters.fmt_detailed(f)?;
        signature
            .display_with(
                self.db,
                env,
                self.settings
                    .disallow_signature_name()
                    .name_already_written(),
            )
            .fmt_detailed(f)
    }
}

impl Display for DisplayOverloadLiteral<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

impl<'db> FunctionType<'db> {
    fn display_with<'env>(
        self,
        db: &'db dyn Db,
        env: &'env ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplayFunctionType<'env, 'db> {
        DisplayFunctionType {
            ty: self,
            db,
            env,
            settings,
        }
    }
}

struct DisplayFunctionType<'env, 'db> {
    ty: FunctionType<'db>,
    db: &'db dyn Db,
    env: &'env ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> FmtDetailed<'db> for DisplayFunctionType<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let env = self.env;
        let db = self.db;
        let Some(settings) = self.settings.enter_function(self.ty) else {
            f.set_invalid_type_annotation();
            f.write_str("def ")?;
            write!(f, "{}", self.ty.name(db))?;
            return f.write_str("(...)");
        };

        let signature = self.ty.signature(db);

        match signature.overloads.as_slice() {
            [signature] => {
                let hide_unused_self = signature.should_hide_self_from_display(db, self.env);

                let type_parameters = DisplayOptionalGenericContext {
                    generic_context: signature.generic_context.as_ref(),
                    db,
                    hide_unused_self,
                };
                f.set_invalid_type_annotation();
                f.write_str("def ")?;
                write!(f, "{}", self.ty.name(db))?;
                type_parameters.fmt_detailed(f)?;
                signature
                    .display_with(
                        self.db,
                        env,
                        settings.disallow_signature_name().name_already_written(),
                    )
                    .fmt_detailed(f)
            }
            signatures => {
                // TODO: How to display overloads?
                if !settings.multiline {
                    // TODO: This should ideally have a TypeDetail but we actually
                    // don't have a type for @overload (we just detect the decorator)
                    f.write_str("Overload")?;
                    f.write_char('[')?;
                }
                let separator = if settings.multiline { "\n" } else { ", " };
                let mut join = f.join(separator);
                for signature in signatures {
                    join.entry(&signature.display_with(db, self.env, settings.clone()));
                }
                join.finish()?;
                if !settings.multiline {
                    f.write_str("]")?;
                }
                Ok(())
            }
        }
    }
}

impl Display for DisplayFunctionType<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

impl<'db> GenericAlias<'db> {
    pub(crate) fn display<'env>(
        self,
        db: &'db dyn Db,
        env: &'env ProgramEnvironment<'db>,
    ) -> DisplayGenericAlias<'env, 'db> {
        self.display_with(db, env, DisplaySettings::default())
    }

    pub(crate) fn display_with<'env>(
        self,
        db: &'db dyn Db,
        env: &'env ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplayGenericAlias<'env, 'db> {
        DisplayGenericAlias {
            origin: ClassLiteral::Static(self.origin(db)),
            specialization: self.specialization(db),
            db,
            env,
            settings,
        }
    }
}

pub(crate) struct DisplayGenericAlias<'env, 'db> {
    origin: ClassLiteral<'db>,
    specialization: Specialization<'db>,
    db: &'db dyn Db,
    env: &'env ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> FmtDetailed<'db> for DisplayGenericAlias<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let env = self.env;
        let db = self.db;
        if let Some(tuple) = self.specialization.tuple(db) {
            tuple
                .display_with(db, self.env, self.settings.clone())
                .fmt_detailed(f)
        } else {
            // basedpython surface syntax: per-typevar use-site variance
            // projections render the keyword (`out`/`in`/`in out`) inline
            // before the corresponding type argument. So a specialization
            // built from `list[out int]` displays as `list[out int]` rather
            // than `list[int]`.
            if basedpython_display_enabled()
                && !self.specialization.projections(self.db).is_empty()
                && self
                    .specialization
                    .projections(self.db)
                    .iter()
                    .any(Option::is_some)
            {
                use ruff_python_ast::helpers::UseSiteVariance;
                self.origin
                    .display_with(self.db, env, self.settings.clone())
                    .fmt_detailed(f)?;
                let types = self.specialization.types(self.db);
                let projections = self.specialization.projections(self.db);
                f.write_char('[')?;
                for (i, ty) in types.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    match projections.get(i).copied().flatten() {
                        Some(UseSiteVariance::Out) => f.write_str("out ")?,
                        Some(UseSiteVariance::In) => f.write_str("in ")?,
                        Some(UseSiteVariance::InOut) => f.write_str("in out ")?,
                        None => {}
                    }
                    ty.display_with(self.db, env, self.settings.clone())
                        .fmt_detailed(f)?;
                }
                return f.write_char(']');
            }

            // basedpython surface syntax: `Top[X[..., Any, ...]]` renders as
            // `X[..., *, ...]` — each invariant typevar that was materialized
            // from `Any`/`Unknown` shows as `*`, concrete typevars render
            // normally. So `Top[dict[str, Any]]` displays as `dict[str, *]`
            if basedpython_display_enabled()
                && matches!(
                    self.specialization.materialization_kind(self.db),
                    Some(MaterializationKind::Top)
                )
            {
                self.origin
                    .display_with(self.db, env, self.settings.clone())
                    .fmt_detailed(f)?;
                let types = self.specialization.types(self.db);
                f.write_char('[')?;
                for (i, ty) in types.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    if matches!(ty, Type::Dynamic(DynamicType::Any | DynamicType::Unknown)) {
                        f.write_char('*')?;
                    } else {
                        ty.display_with(self.db, env, self.settings.clone())
                            .fmt_detailed(f)?;
                    }
                }
                return f.write_char(']');
            }
            let prefix_details = match self.specialization.materialization_kind(self.db) {
                None => None,
                Some(MaterializationKind::Top) => Some(("Top", SpecialFormType::Top)),
                Some(MaterializationKind::Bottom) => Some(("Bottom", SpecialFormType::Bottom)),
            };
            let suffix = match self.specialization.materialization_kind(db) {
                None => "",
                Some(_) => "]",
            };
            if let Some((name, form)) = prefix_details {
                f.with_type(Type::SpecialForm(form)).write_str(name)?;
                f.write_char('[')?;
            }
            self.origin
                .display_with(db, env, self.settings.clone())
                .fmt_detailed(f)?;
            self.specialization
                .display_short(
                    db,
                    self.env,
                    TupleSpecialization::from_class(db, self.origin),
                    self.settings.clone(),
                )
                .fmt_detailed(f)?;
            f.write_str(suffix)
        }
    }
}

impl Display for DisplayGenericAlias<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

impl<'db> GenericContext<'db> {
    fn display<'a>(&'a self, db: &'db dyn Db) -> DisplayGenericContext<'a, 'db> {
        DisplayGenericContext {
            generic_context: self,
            db,
            full: false,
            hide_unused_self: false,
        }
    }

    fn display_full<'a>(&'a self, db: &'db dyn Db) -> DisplayGenericContext<'a, 'db> {
        DisplayGenericContext {
            generic_context: self,
            db,
            full: true,
            hide_unused_self: false,
        }
    }
}

/// basedpython: what a parameter's own anonymous hole bounds it by, when its annotated type is
/// one. See [`DisplayParameter`].
enum SomeHoleBound<'db> {
    Bounded(Type<'db>),
    /// nothing was ever required of it, so `some` has nothing to say
    Unbounded,
}

/// basedpython: the bound of the `some` hole `ty` is, when it is one.
fn some_hole_bound<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<SomeHoleBound<'db>> {
    let Type::TypeVar(bound_typevar) = ty else {
        return None;
    };
    let typevar = bound_typevar.typevar(db);
    if !typevar.is_some_hole(db) {
        return None;
    }
    Some(match typevar.upper_bound(db, env) {
        Some(bound) if !bound.is_dynamic() => SomeHoleBound::Bounded(bound),
        _ => SomeHoleBound::Unbounded,
    })
}

/// basedpython: a synthesized protocol spelled as the inline `protocol(...)` type expression
/// that would declare it — `protocol(a: int; def m(self) -> str)`.
struct DisplayInlineProtocol<'env, 'db> {
    db: &'db dyn Db,
    env: &'env ProgramEnvironment<'db>,
    interface: ProtocolInterface<'db>,
    settings: DisplaySettings<'db>,
}

impl<'env, 'db> DisplayInlineProtocol<'env, 'db> {
    /// `None` when some part of the interface has no inline spelling, so that the protocol is
    /// described rather than spelled wrongly.
    fn new(
        db: &'db dyn Db,
        env: &'env ProgramEnvironment<'db>,
        interface: ProtocolInterface<'db>,
        settings: DisplaySettings<'db>,
    ) -> Option<Self> {
        interface
            .members(db)
            .all(|member| member.inline_form().is_some())
            .then_some(Self {
                db,
                env,
                interface,
                settings,
            })
    }
}

impl<'db> FmtDetailed<'db> for DisplayInlineProtocol<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let env = self.env;
        f.write_str("protocol(")?;
        let mut first = true;
        for member in self.interface.members(self.db) {
            if !std::mem::take(&mut first) {
                f.write_str("; ")?;
            }
            match member.inline_form() {
                Some(InlineProtocolMemberForm::Method(callable)) => {
                    write!(f, "def {}", member.name())?;
                    callable
                        .display_with(self.db, env, self.settings.clone())
                        .fmt_detailed(f)?;
                }
                Some(InlineProtocolMemberForm::Attribute(ty)) => {
                    write!(f, "{}: ", member.name())?;
                    ty.display_with(self.db, env, self.settings.clone())
                        .fmt_detailed(f)?;
                }
                // `new` rejected an interface with such a member
                None => return Err(fmt::Error),
            }
        }
        for pack in self.interface.pending_packs(self.db) {
            if !std::mem::take(&mut first) {
                f.write_str("; ")?;
            }
            write!(f, "**{}", pack.display(self.db, env))?;
        }
        f.write_char(')')
    }
}

struct DisplayOptionalGenericContext<'a, 'db> {
    generic_context: Option<&'a GenericContext<'db>>,
    db: &'db dyn Db,
    /// If true, hide `Self` type variables from the generic context prefix
    /// when they are not displayed in the signature body.
    hide_unused_self: bool,
}

impl<'db> FmtDetailed<'db> for DisplayOptionalGenericContext<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        if let Some(generic_context) = self.generic_context {
            DisplayGenericContext {
                generic_context,
                db: self.db,
                full: false,
                hide_unused_self: self.hide_unused_self,
            }
            .fmt_detailed(f)
        } else {
            Ok(())
        }
    }
}

impl Display for DisplayOptionalGenericContext<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

struct DisplayGenericContext<'a, 'db> {
    generic_context: &'a GenericContext<'db>,
    db: &'db dyn Db,
    full: bool,
    /// If true, hide `Self` type variables from the generic context prefix.
    hide_unused_self: bool,
}

impl<'db> DisplayGenericContext<'_, 'db> {
    fn fmt_normal(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let mut variables = self
            .generic_context
            .variables(self.db)
            .filter(|bound_typevar| {
                // If hide_unused_self is true and this is a Self typevar, skip it
                !self.hide_unused_self || !bound_typevar.typevar(self.db).is_self(self.db)
            })
            // basedpython: an anonymous hole is not an entry a call site can supply, and it is
            // shown where it was opened — as the `some` on its own parameter
            .filter(|bound_typevar| !bound_typevar.typevar(self.db).is_some_hole(self.db))
            .peekable();

        if variables.peek().is_none() {
            return Ok(());
        }

        f.write_char('[')?;
        for (idx, bound_typevar) in variables.enumerate() {
            if idx > 0 {
                f.write_str(", ")?;
            }
            f.set_invalid_type_annotation();
            let typevar = bound_typevar.typevar(self.db);
            if typevar.is_parameter_pack(self.db) {
                f.write_str("**")?;
            } else if typevar.is_typevartuple(self.db) {
                f.write_char('*')?;
            }
            write!(
                f.with_type(Type::TypeVar(bound_typevar)),
                "{}",
                typevar.name(self.db)
            )?;
        }
        f.write_char(']')?;

        Ok(())
    }

    fn fmt_full(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let variables = self.generic_context.variables(self.db);
        f.write_char('[')?;
        for (idx, bound_typevar) in variables.enumerate() {
            if idx > 0 {
                f.write_str(", ")?;
            }
            f.set_invalid_type_annotation();
            write!(
                f.with_type(Type::TypeVar(bound_typevar)),
                "{}",
                bound_typevar.identity(self.db).display(self.db)
            )?;
        }
        f.write_char(']')
    }
}

impl<'db> FmtDetailed<'db> for DisplayGenericContext<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        if self.full {
            self.fmt_full(f)
        } else {
            self.fmt_normal(f)
        }
    }
}

impl Display for DisplayGenericContext<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

impl<'db> Specialization<'db> {
    fn display_full<'env>(
        self,
        db: &'db dyn Db,
        env: &'env ProgramEnvironment<'db>,
    ) -> DisplaySpecialization<'env, 'db> {
        DisplaySpecialization {
            specialization: self,
            db,
            env,
            tuple_specialization: TupleSpecialization::No,
            settings: DisplaySettings::default(),
            full: true,
        }
    }

    /// Renders the specialization as it would appear in a subscript expression, e.g. `[int, str]`.
    fn display_short<'env>(
        self,
        db: &'db dyn Db,
        env: &'env ProgramEnvironment<'db>,
        tuple_specialization: TupleSpecialization,
        settings: DisplaySettings<'db>,
    ) -> DisplaySpecialization<'env, 'db> {
        DisplaySpecialization {
            specialization: self,
            db,
            env,
            tuple_specialization,
            settings,
            full: false,
        }
    }
}

struct DisplaySpecialization<'env, 'db> {
    specialization: Specialization<'db>,
    db: &'db dyn Db,
    env: &'env ProgramEnvironment<'db>,
    tuple_specialization: TupleSpecialization,
    settings: DisplaySettings<'db>,
    full: bool,
}

impl<'db> DisplaySpecialization<'_, 'db> {
    fn fmt_normal(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let env = self.env;
        let db = self.db;
        f.write_char('[')?;
        let variables = self
            .specialization
            .generic_context(db)
            .variables(db)
            .collect::<Vec<_>>();
        let types = self.specialization.types(db);
        let mut wrote_any = false;
        for (typevar, ty) in variables.iter().zip(types) {
            if typevar.is_typevartuple(db) {
                let Some(tuple) = ty.exact_tuple_instance_spec(db) else {
                    if wrote_any {
                        f.write_str(", ")?;
                    }
                    ty.display_with(db, self.env, self.settings.clone())
                        .fmt_detailed(f)?;
                    wrote_any = true;
                    continue;
                };
                match tuple.as_ref() {
                    TupleSpec::Fixed(fixed) if fixed.elements_slice().is_empty() => {
                        if variables.len() == 1 {
                            if wrote_any {
                                f.write_str(", ")?;
                            }
                            f.write_str("()")?;
                            wrote_any = true;
                        }
                    }
                    TupleSpec::Fixed(fixed) => {
                        for element in fixed.elements_slice() {
                            if wrote_any {
                                f.write_str(", ")?;
                            }
                            element
                                .display_with(db, self.env, self.settings.clone())
                                .fmt_detailed(f)?;
                            wrote_any = true;
                        }
                    }
                    TupleSpec::Variable(_) => {
                        if wrote_any {
                            f.write_str(", ")?;
                        }
                        f.write_char('*')?;
                        ty.display_with(db, self.env, self.settings.clone())
                            .fmt_detailed(f)?;
                        wrote_any = true;
                    }
                }
                continue;
            }

            // basedpython: a keyword-variadic pack reads back the way it is written —
            // `A[foo=int, bar=str]` — rather than as the parameter list that stores it
            if typevar.is_keyword_variadic(self.db)
                && let Some(fields) = ty.keyword_pack_fields(self.db)
            {
                if fields.is_empty() {
                    if variables.len() == 1 {
                        if wrote_any {
                            f.write_str(", ")?;
                        }
                        f.write_str("()")?;
                        wrote_any = true;
                    }
                    continue;
                }
                for (name, field_type) in fields {
                    if wrote_any {
                        f.write_str(", ")?;
                    }
                    write!(f, "{name}=")?;
                    field_type
                        .display_with(self.db, env, self.settings.clone())
                        .fmt_detailed(f)?;
                    wrote_any = true;
                }
                continue;
            }

            if wrote_any {
                f.write_str(", ")?;
            }
            // a lone type parameter has nothing to disambiguate, so naming it
            // would be noise rather than orientation
            if self.settings.name_type_arguments && variables.len() > 1 {
                write!(f, "{}=", typevar.name(self.db))?;
            }
            ty.display_with(self.db, env, self.settings.clone())
                .fmt_detailed(f)?;
            wrote_any = true;
        }
        if self.tuple_specialization.is_yes() {
            f.write_str(", ...")?;
        }
        f.write_char(']')
    }

    fn fmt_full(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let db = self.db;
        f.write_char('[')?;
        let variables = self.specialization.generic_context(db).variables(db);
        let types = self.specialization.types(db);
        for (idx, (bound_typevar, ty)) in variables.zip(types).enumerate() {
            if idx > 0 {
                f.write_str(", ")?;
            }
            f.set_invalid_type_annotation();
            write!(f, "{}", bound_typevar.identity(db).display(db))?;
            f.write_str(" = ")?;
            ty.display_with(db, self.env, self.settings.clone())
                .fmt_detailed(f)?;
        }
        f.write_char(']')
    }
}

impl<'db> FmtDetailed<'db> for DisplaySpecialization<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        if self.full {
            self.fmt_full(f)
        } else {
            self.fmt_normal(f)
        }
    }
}

impl Display for DisplaySpecialization<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TupleSpecialization {
    Yes,
    No,
}

impl TupleSpecialization {
    const fn is_yes(self) -> bool {
        matches!(self, Self::Yes)
    }

    fn from_class(db: &dyn Db, class: ClassLiteral) -> Self {
        if class.is_tuple(db) {
            Self::Yes
        } else {
            Self::No
        }
    }
}

impl<'db> CallableType<'db> {
    fn display_with<'a>(
        &'a self,
        db: &'db dyn Db,
        env: &'a ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplayCallableType<'a, 'db> {
        DisplayCallableType {
            signatures: self.signatures(db),
            kind: self.kind(db),
            db,
            env,
            settings,
        }
    }
}

pub(crate) struct DisplayCallableType<'a, 'db> {
    signatures: &'a CallableSignature<'db>,
    kind: CallableTypeKind,
    db: &'db dyn Db,
    env: &'a ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> FmtDetailed<'db> for DisplayCallableType<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let db = self.db;
        match self.signatures.overloads.as_slice() {
            [signature] => {
                if matches!(self.kind, CallableTypeKind::ParamSpecValue) {
                    if signature.parameters().is_top() {
                        f.write_str("Top[")?;
                    }
                    signature
                        .parameters()
                        .display_with(db, self.env, self.settings.clone())
                        .fmt_detailed(f)?;
                    if signature.parameters().is_top() {
                        f.write_str("]")?;
                    }
                } else {
                    signature
                        .display_with(db, self.env, self.settings.clone())
                        .fmt_detailed(f)?;
                }
            }
            signatures => {
                // TODO: How to display overloads?
                if !self.settings.multiline {
                    // TODO: This should ideally have a TypeDetail but we actually
                    // don't have a type for @overload (we just detect the decorator)
                    f.write_str("Overload")?;
                    f.write_char('[')?;
                }
                let separator = if self.settings.multiline { "\n" } else { ", " };
                let mut join = f.join(separator);
                for signature in signatures {
                    join.entry(&signature.display_with(db, self.env, self.settings.clone()));
                }
                join.finish()?;
                if !self.settings.multiline {
                    f.write_char(']')?;
                }
            }
        }

        Ok(())
    }
}

impl Display for DisplayCallableType<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

impl<'db> Signature<'db> {
    /// Displays this signature with qualification inferred across all parameter and return types.
    ///
    /// For example, considering the annotations together keeps the two `float` classes distinct:
    ///
    /// ```python
    /// import builtins
    ///
    /// class float: ...
    ///
    /// def f(value: builtins.float | float) -> None: ...
    /// ```
    pub(crate) fn display<'a>(
        &'a self,
        db: &'db dyn Db,
        env: &'a ProgramEnvironment<'db>,
    ) -> DisplaySignature<'a, 'db> {
        Self::display_with(
            self,
            db,
            env,
            DisplaySettings::from_possibly_ambiguous_types(
                db,
                env,
                self.parameters()
                    .iter()
                    .map(Parameter::annotated_type)
                    .chain(std::iter::once(self.return_ty)),
            ),
        )
    }

    pub(crate) fn display_with<'a>(
        &'a self,
        db: &'db dyn Db,
        env: &'a ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplaySignature<'a, 'db> {
        DisplaySignature {
            definition: self.definition(),
            generic_context: self.generic_context.as_ref(),
            parameters: self.parameters(),
            return_ty: self.return_ty,
            db,
            env,
            settings,
        }
    }
}

pub(crate) struct DisplaySignature<'a, 'db> {
    definition: Option<Definition<'db>>,
    generic_context: Option<&'a GenericContext<'db>>,
    parameters: &'a Parameters<'db>,
    return_ty: Type<'db>,
    db: &'db dyn Db,
    env: &'a ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

impl DisplaySignature<'_, '_> {
    #[must_use]
    pub(crate) fn multiline(self) -> Self {
        Self {
            settings: self.settings.multiline(),
            ..self
        }
    }

    #[must_use]
    pub(crate) fn disallow_name(self) -> Self {
        Self {
            settings: self.settings.disallow_signature_name(),
            ..self
        }
    }

    #[must_use]
    pub(crate) fn hide_return_type(self) -> Self {
        Self {
            settings: self.settings.hide_return_type(),
            ..self
        }
    }

    /// Get detailed display information including component ranges
    pub(crate) fn to_string_parts(&self) -> SignatureDisplayDetails {
        let mut f = TypeWriter::Details(TypeDetailsWriter::new());
        self.fmt_detailed(&mut f).unwrap();

        match f {
            TypeWriter::Details(details) => details.finish_signature_details(),
            TypeWriter::Formatter(_) => unreachable!("Expected Details variant"),
        }
    }

    fn should_hide_self_from_display(&self) -> bool {
        let db = self.db;
        let env = self.env;

        !self.return_ty.contains_self(db, env)
            && !self.parameters.iter().any(|p| {
                p.should_annotation_be_displayed() && p.annotated_type().contains_self(db, env)
            })
    }
}

impl<'db> FmtDetailed<'db> for DisplaySignature<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let env = self.env;
        let db = self.db;
        // Immediately write a marker signaling we're starting a signature
        let _ = f.with_detail(TypeDetail::SignatureStart);
        f.set_invalid_type_annotation();
        // When we exit this function, write a marker signaling we're ending a signature
        let mut f = f.with_detail(TypeDetail::SignatureEnd);

        if self.parameters.is_top() {
            f.write_str("Top[")?;
        }

        // If the current display policy wants a signature name and a name hasn't been emitted,
        // remember what the name was by checking if we have a definition
        let mut is_declaration = self.settings.name_already_written;
        if self
            .settings
            .signature_name_display
            .should_display(self.settings.multiline)
            && let Some(definition) = self.definition
            && let Some(name) = definition.name(db)
        {
            f.write_str("def ")?;
            f.write_str(&name)?;
            is_declaration = true;
        }

        let settings = self.settings.with_generic_context(db, self.generic_context);

        // Display type parameters if present, but only when the caller hasn't
        // already displayed them.
        if self
            .settings
            .signature_name_display
            .allows_type_parameters()
        {
            let hide_unused_self = self.should_hide_self_from_display();

            DisplayOptionalGenericContext {
                generic_context: self.generic_context,
                db,
                hide_unused_self,
            }
            .fmt_detailed(&mut f)?;
        }

        // Parameters. Whatever is nested in here is a *type*, not this declaration, so it does
        // not inherit the right to leave out a `None` return
        let param_settings = DisplaySettings {
            hide_return_type: false,
            name_already_written: false,
            ..settings.clone()
        };
        self.parameters
            .display_with(db, self.env, param_settings)
            .fmt_detailed(&mut f)?;

        // Return type.
        //
        // basedpython: a `def` that says nothing returns `None`, so spelling it out is noise —
        // `def f()` and `def f() -> None` are the same declaration. A callable *type* has no
        // such default and always states what it returns, or `() -> None` would read as `()`
        let omit_none_return = is_declaration && self.return_ty.is_none(self.db);
        if !self.settings.hide_return_type && !omit_none_return {
            f.write_str(" -> ")?;

            let should_parenthesize_return_type =
                should_parenthesize_callable_type(self.return_ty, db);
            if should_parenthesize_return_type {
                f.write_char('(')?;
            }
            self.return_ty
                .display_with(
                    self.db,
                    env,
                    DisplaySettings {
                        name_already_written: false,
                        ..settings.singleline()
                    },
                )
                .fmt_detailed(&mut f)?;
            if should_parenthesize_return_type {
                f.write_char(')')?;
            }
        }

        if self.parameters.is_top() {
            f.write_str("]")?;
        }

        Ok(())
    }
}

impl Display for DisplaySignature<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

/// Details about signature display components, including ranges for parameters and return type
#[derive(Debug, Clone)]
pub(crate) struct SignatureDisplayDetails {
    /// The full signature string
    pub label: String,
    /// Ranges for each parameter within the label
    pub parameter_ranges: Vec<TextRange>,
    /// Names of the parameters in order
    pub parameter_names: Vec<String>,
}

impl<'db> Parameters<'db> {
    fn display_with<'a>(
        &'a self,
        db: &'db dyn Db,
        env: &'a ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplayParameters<'a, 'db> {
        DisplayParameters {
            parameters: self,
            db,
            env,
            settings,
        }
    }
}

struct DisplayParameters<'a, 'db> {
    parameters: &'a Parameters<'db>,
    db: &'db dyn Db,
    env: &'a ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> FmtDetailed<'db> for DisplayParameters<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        fn display_parameters<'db>(
            display: &DisplayParameters<'_, 'db>,
            f: &mut TypeWriter<'_, '_, 'db>,
            parameters: &[Parameter<'db>],
            arg_separator: &str,
        ) -> fmt::Result {
            let db = display.db;
            let mut star_added = false;
            let mut needs_slash = false;
            let mut after_synthetic_unpack = false;
            let mut first = true;

            for parameter in parameters {
                let is_synthetic_unpack = parameter.definition().is_none()
                    && parameter.is_variadic()
                    && parameter.has_starred_annotation();

                // Handle special separators
                if parameter.is_positional_only() && !after_synthetic_unpack {
                    needs_slash = true;
                } else if needs_slash {
                    if !first {
                        f.write_str(arg_separator)?;
                    }
                    f.write_char('/')?;
                    needs_slash = false;
                    first = false;
                }
                if !star_added && parameter.is_keyword_only() {
                    if !first {
                        f.write_str(arg_separator)?;
                    }
                    f.write_char('*')?;
                    star_added = true;
                    first = false;
                }

                // Add comma before parameter if not first
                if !first {
                    f.write_str(arg_separator)?;
                }

                // Write parameter with range tracking
                let param_name = parameter
                    .display_name()
                    .map(|name| name.to_string())
                    .unwrap_or_default();
                parameter
                    .display_with(db, display.env, display.settings.singleline())
                    .fmt_detailed(&mut f.with_detail(TypeDetail::Parameter(param_name)))?;

                after_synthetic_unpack |= is_synthetic_unpack;
                star_added |= parameter.is_variadic();
                first = false;
            }

            if needs_slash {
                if !first {
                    f.write_str(arg_separator)?;
                }
                f.write_char('/')?;
            }

            Ok(())
        }
        let db = self.db;

        // For `ParamSpec` kind, the parameters still contain `*args` and `**kwargs`, but we
        // display them as `**P` instead, so avoid multiline in that case.
        // For `Concatenate` kind, use multiline only if there are more than 1 prefix parameters.
        // For `Gradual` kind without prefix params (len <= 2), display as `...`.
        let multiline = if self.settings.multiline {
            match self.parameters.kind() {
                ParametersKind::Standard => self.parameters.len() > 1,
                ParametersKind::Gradual | ParametersKind::Top | ParametersKind::ParamSpec(_) => {
                    false
                }
                ParametersKind::Concatenate(_) => {
                    // The tail already represents 2 parameters. Additionally, there should be more
                    // than 1 prefix parameters to use multiline, so the limit becomes 3.
                    self.parameters.len() > 3
                }
            }
        } else {
            false
        };

        // Opening parenthesis
        f.write_char('(')?;
        if multiline {
            f.write_str("\n    ")?;
        }

        let arg_separator = if multiline { ",\n    " } else { ", " };

        match self.parameters.kind() {
            ParametersKind::Standard | ParametersKind::Concatenate(_) => {
                display_parameters(self, f, self.parameters.as_slice(), arg_separator)?;
            }
            ParametersKind::Top => {
                // TODO: Remove `...`, always display all the parameters
                // Top parameters are displayed the same as gradual parameters, we just wrap the
                // entire signature in `Top[]`
                f.write_str("...")?;
            }
            ParametersKind::Gradual if self.parameters.len() == 2 => {
                // TODO: Remove `...`, always display all the parameters
                // For gradual parameters with only `(*args, **kwargs)`, display as `...` for
                // simplicity ...
                f.write_str("...")?;
            }
            ParametersKind::Gradual => {
                // ... but otherwise display all the parameters as normal.
                display_parameters(self, f, self.parameters.as_slice(), arg_separator)?;
            }
            ParametersKind::ParamSpec(typevar) => {
                let parameter_name = format!("**{}", typevar.name(db));
                let mut parameter = f.with_detail(TypeDetail::Parameter(parameter_name.clone()));
                write!(parameter, "{parameter_name}")?;
                let binding_context = typevar.binding_context(db);
                if let Some(binding_context_name) = binding_context.name(db)
                    && let Some(definition) = binding_context.definition()
                    && !self.settings.active_scopes.contains(&definition)
                {
                    write!(parameter, "@{binding_context_name}")?;
                }
            }
        }

        if multiline {
            f.write_char('\n')?;
        }

        // Closing parenthesis
        f.write_char(')')
    }
}

impl Display for DisplayParameters<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

impl<'db> Parameter<'db> {
    fn display_with<'a>(
        &'a self,
        db: &'db dyn Db,
        env: &'a ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplayParameter<'a, 'db> {
        DisplayParameter {
            param: self,
            db,
            env,
            settings,
        }
    }
}

struct DisplayParameter<'a, 'db> {
    param: &'a Parameter<'db>,
    db: &'db dyn Db,
    env: &'a ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> FmtDetailed<'db> for DisplayParameter<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let env = self.env;
        let db = self.db;
        if self.param.definition().is_none()
            && self.param.is_variadic()
            && self.param.has_starred_annotation()
        {
            f.write_str("*")?;
            self.param
                .annotated_type()
                .display_with(db, self.env, self.settings.clone())
                .fmt_detailed(f)?;
            return Ok(());
        }

        if let Some(name) = self.param.display_name() {
            write!(f, "{name}")?;
            if self.param.should_annotation_be_displayed() {
                let annotated_type = self.param.annotated_type();
                // basedpython: the hole this parameter opened is spelled where it was
                // opened — `some <bound>` — rather than as a type parameter of its own. an
                // unbounded hole has nothing to say, so the parameter reads as unannotated
                match some_hole_bound(self.db, env, annotated_type) {
                    Some(SomeHoleBound::Bounded(bound)) => {
                        f.write_str(": some ")?;
                        bound
                            .display_with(self.db, env, self.settings.clone())
                            .fmt_detailed(f)?;
                    }
                    Some(SomeHoleBound::Unbounded) => {}
                    None => {
                        f.write_str(": ")?;
                        if self.param.is_variadic() && self.param.has_starred_annotation() {
                            f.write_char('*')?;
                        }
                        annotated_type
                            .display_with(self.db, env, self.settings.clone())
                            .fmt_detailed(f)?;
                    }
                }
            }
            // Default value can only be specified if `name` is given.
            if let Some(default_type) = self.param.default_type(db) {
                if self.param.should_annotation_be_displayed() {
                    f.write_str(" = ")?;
                } else {
                    f.write_str("=")?;
                }
                match default_type {
                    Type::LiteralValue(literal)
                        if matches!(
                            literal.kind(),
                            LiteralValueTypeKind::Int(_)
                                | LiteralValueTypeKind::Bool(_)
                                | LiteralValueTypeKind::String(_)
                                | LiteralValueTypeKind::Enum(_)
                                | LiteralValueTypeKind::Bytes(_)
                        ) =>
                    {
                        // For Literal types display the value without `Literal[..]` wrapping
                        let representation =
                            default_type.representation(db, self.env, self.settings.clone());
                        representation.fmt_detailed(f)?;
                    }
                    Type::NominalInstance(instance) => {
                        // Some key default types like `None` are worth showing
                        let class = instance.class(db, self.env);

                        match (class, class.known(db)) {
                            (_, Some(KnownClass::NoneType)) => {
                                f.with_type(default_type).write_str("None")?;
                            }
                            (_, Some(KnownClass::NoDefaultType)) => {
                                f.with_type(default_type).write_str("NoDefault")?;
                            }
                            _ => f.write_str("...")?,
                        }
                    }
                    _ => f.write_str("...")?,
                }
            }
        } else {
            // This case is specifically for the `Callable` signature where name and default value
            // cannot be provided. For unnamed parameters we always display the type, to ensure we
            // have something visible in the parameter slot.
            self.param
                .annotated_type()
                .display_with(db, self.env, self.settings.clone())
                .fmt_detailed(f)?;
        }
        Ok(())
    }
}

impl Display for DisplayParameter<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

#[derive(Debug, Copy, Clone)]
struct TruncationPolicy {
    max: usize,
    max_when_elided: usize,
}

impl TruncationPolicy {
    fn display_limit(self, total: usize, preserve_full: bool) -> usize {
        if preserve_full {
            return total;
        }
        let limit = if total > self.max {
            self.max_when_elided
        } else {
            self.max
        };
        limit.min(total)
    }
}

#[derive(Debug)]
struct DisplayOmitted {
    count: usize,
    singular: &'static str,
    plural: &'static str,
}

impl<'db> FmtDetailed<'db> for DisplayOmitted {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let noun = if self.count == 1 {
            self.singular
        } else {
            self.plural
        };
        f.set_invalid_type_annotation();
        write!(f, "... omitted {} {}", self.count, noun)
    }
}

impl Display for DisplayOmitted {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

impl<'db> UnionType<'db> {
    fn display_with<'a>(
        &'a self,
        db: &'db dyn Db,
        env: &'a ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplayUnionType<'a, 'db> {
        DisplayUnionType {
            db,
            env,
            ty: self,
            settings,
        }
    }
}

struct DisplayUnionType<'a, 'db> {
    ty: &'a UnionType<'db>,
    db: &'db dyn Db,
    env: &'a ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> DisplayUnionType<'_, 'db> {
    /// Return the literal types that can be folded into a displayed `Literal[...]` group.
    ///
    /// Plain literal types are returned as-is. Small enum complements are expanded to their
    /// remaining enum literals so a type like `Color & ~Literal[Color.RED]` can be displayed
    /// with the same condensation rules as explicit enum-literal unions. Large complements
    /// stay compact to keep diagnostics readable.
    ///
    /// ```python
    /// from enum import Enum
    ///
    /// class Color(Enum):
    ///     RED = 1
    ///     BLUE = 2
    ///
    /// # Color excluding RED displays through the literal-group path for BLUE.
    /// ```
    fn condensable_literals(&self, ty: Type<'db>) -> Option<Vec<Type<'db>>> {
        // basedpython displays each union element separately in source order, so
        // nothing is condensed into a `Literal[...]` group
        if basedpython_display_enabled() {
            return None;
        }
        match ty {
            Type::LiteralValue(literal)
                if matches!(
                    literal.kind(),
                    LiteralValueTypeKind::Int(_)
                        | LiteralValueTypeKind::String(_)
                        | LiteralValueTypeKind::Bytes(_)
                        | LiteralValueTypeKind::Bool(_)
                        | LiteralValueTypeKind::Enum(_)
                ) =>
            {
                Some(vec![ty])
            }
            Type::EnumComplement(complement) => complement.remaining_literal_types_for_display(
                self.db,
                self.env,
                LITERAL_POLICY.max,
            ),
            Type::Intersection(intersection) => {
                intersection.finite_alternatives_for_display(self.db, self.env, LITERAL_POLICY.max)
            }
            _ => None,
        }
    }
}

const UNION_POLICY: TruncationPolicy = TruncationPolicy {
    max: 5,
    max_when_elided: 3,
};

fn subclass_of_known_class(db: &dyn Db, subclass_of: SubclassOfType<'_>) -> Option<KnownClass> {
    match subclass_of.subclass_of() {
        SubclassOfInner::Class(class) => class.known(db),
        _ => None,
    }
}

impl<'db> FmtDetailed<'db> for DisplayUnionType<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        fn singleline_union_element_label<'db>(
            db: &'db dyn Db,
            env: &ProgramEnvironment<'db>,
            element: Type<'db>,
            settings: &DisplaySettings<'db>,
        ) -> String {
            element
                .display_with(db, env, settings.singleline())
                .to_string()
        }

        fn duplicate_ambiguous_labels(element_labels: &[Option<String>]) -> FxHashSet<&str> {
            let mut counts: FxHashMap<&str, usize> = FxHashMap::default();

            for label in element_labels.iter().flatten() {
                *counts.entry(&**label).or_default() += 1;
            }

            counts
                .into_iter()
                .filter_map(|(label, count)| (count > 1).then_some(label))
                .collect()
        }
        let db = self.db;

        let elements = self.ty.elements(db);
        let numeric_tower: Option<KnownUnion> = None;
        let is_numeric_tower_element = |element: Type<'db>| {
            numeric_tower.is_some_and(|group| {
                element
                    .as_nominal_instance()
                    .and_then(|instance| instance.known_class(db))
                    .is_some_and(|known_class| group.contains(known_class))
            })
        };
        let mut condensed_types = vec![];
        let mut condensed_element_count = 0usize;
        let mut subclass_of_types = vec![];
        let element_labels: Vec<_> = elements
            .iter()
            .copied()
            .map(|element| {
                (self.condensable_literals(element).is_none()
                    && !element.is_subclass_of()
                    && !is_numeric_tower_element(element))
                .then(|| singleline_union_element_label(db, self.env, element, &self.settings))
            })
            .collect();
        let duplicate_ambiguous_labels = duplicate_ambiguous_labels(&element_labels);

        for element in elements.iter().copied() {
            if let Some(literals) = self.condensable_literals(element) {
                condensed_element_count += 1;
                for literal in literals {
                    if !condensed_types.contains(&literal) {
                        condensed_types.push(literal);
                    }
                }
            } else if let Type::SubclassOf(subclass_of) = element {
                subclass_of_types.push(subclass_of);
            }
        }

        let numeric_tower_element_count = elements
            .iter()
            .copied()
            .filter(|element| is_numeric_tower_element(*element))
            .count();
        let total_entries = elements.len()
            - numeric_tower_element_count
            - condensed_element_count
            - subclass_of_types.len()
            + usize::from(numeric_tower.is_some())
            + usize::from(!condensed_types.is_empty())
            + usize::from(!subclass_of_types.is_empty());

        assert_ne!(total_entries, 0);

        // Done manually because we have a mix of FmtDetailed and Display
        let mut join = f.join(" | ");

        let display_limit =
            UNION_POLICY.display_limit(total_entries, self.settings.preserve_full_unions);

        let mut numeric_tower = numeric_tower;
        let mut condensed_types = Some(condensed_types);
        let mut subclass_of_types = Some(subclass_of_types);
        let mut displayed_entries = 0usize;

        for (element, label) in elements.iter().zip(&element_labels) {
            if displayed_entries >= display_limit {
                break;
            }

            if is_numeric_tower_element(*element) {
                if let Some(union) = numeric_tower.take() {
                    displayed_entries += 1;
                    join.entry(&DisplayKnownUnion {
                        union,
                        db,
                        env: self.env,
                        settings: self.settings.singleline(),
                    });
                }
            } else if self.condensable_literals(*element).is_some() {
                if let Some(literals) = condensed_types.take() {
                    displayed_entries += 1;
                    join.entry(&DisplayLiteralGroup {
                        literals,
                        db,
                        env: self.env,
                        settings: self.settings.singleline(),
                    });
                }
            } else if element.is_subclass_of() {
                if let Some(types) = subclass_of_types.take() {
                    displayed_entries += 1;
                    join.entry(&DisplaySubclassOfGroup {
                        types,
                        db,
                        env: self.env,
                        settings: self.settings.singleline(),
                    });
                }
            } else {
                displayed_entries += 1;
                let settings = if label
                    .as_deref()
                    .is_some_and(|label| duplicate_ambiguous_labels.contains(label))
                {
                    self.settings.singleline().force_signature_name()
                } else {
                    self.settings.singleline()
                };
                join.entry(&DisplayMaybeParenthesizedType {
                    ty: *element,
                    db,
                    env: self.env,
                    settings,
                });
            }
        }

        if !self.settings.preserve_full_unions {
            let omitted_entries = total_entries.saturating_sub(displayed_entries);
            if omitted_entries > 0 {
                join.entry(&DisplayOmitted {
                    count: omitted_entries,
                    singular: "union element",
                    plural: "union elements",
                });
            }
        }
        join.finish()
    }
}

/// Displays a numeric-tower union through its canonical annotation class.
///
/// Delegating to the class display preserves qualification and IDE navigation metadata.
struct DisplayKnownUnion<'env, 'db> {
    union: KnownUnion,
    db: &'db dyn Db,
    env: &'env ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> FmtDetailed<'db> for DisplayKnownUnion<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let env = self.env;
        let class = self.union.annotation_class();
        if let Some(class_literal) = class.try_to_class_literal(self.db, self.env) {
            ClassLiteral::Static(class_literal)
                .display_with(self.db, env, self.settings.clone())
                .fmt_detailed(f)
        } else {
            f.with_type(class.to_instance(self.db, self.env))
                .write_str(class.name(self.env.python_version(self.db)))
        }
    }
}

impl Display for DisplayUnionType<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

impl fmt::Debug for DisplayUnionType<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, f)
    }
}

struct DisplaySubclassOfGroup<'env, 'db> {
    types: Vec<SubclassOfType<'db>>,
    db: &'db dyn Db,
    env: &'env ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> FmtDetailed<'db> for DisplaySubclassOfGroup<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let env = self.env;
        let db = self.db;
        f.write_str("type[")?;
        let numeric_tower: Option<KnownUnion> = None;
        let is_numeric_tower_subclass = |subclass_of: SubclassOfType<'db>| {
            numeric_tower.is_some_and(|group| {
                subclass_of_known_class(self.db, subclass_of)
                    .is_some_and(|known_class| group.contains(known_class))
            })
        };
        let numeric_tower_element_count = self
            .types
            .iter()
            .copied()
            .filter(|subclass_of| is_numeric_tower_subclass(*subclass_of))
            .count();
        let total_entries =
            self.types.len() - numeric_tower_element_count + usize::from(numeric_tower.is_some());
        let display_limit =
            UNION_POLICY.display_limit(total_entries, self.settings.preserve_full_unions);
        let mut join = f.join(" | ");
        let mut numeric_tower = numeric_tower;
        let mut displayed_entries = 0usize;

        for subclass_of in &self.types {
            if displayed_entries >= display_limit {
                break;
            }

            if is_numeric_tower_subclass(*subclass_of) {
                if let Some(union) = numeric_tower.take() {
                    displayed_entries += 1;
                    join.entry(&DisplayKnownUnion {
                        union,
                        db,
                        env: self.env,
                        settings: self.settings.singleline(),
                    });
                }
                continue;
            }

            displayed_entries += 1;

            match subclass_of.subclass_of() {
                SubclassOfInner::Class(ClassType::NonGeneric(class)) => {
                    join.entry(&class.display_with(db, env, self.settings.singleline()));
                }
                SubclassOfInner::Class(ClassType::Generic(alias)) => {
                    join.entry(&alias.display_with(db, self.env, self.settings.singleline()));
                }
                SubclassOfInner::Dynamic(dynamic) => {
                    let rep = Type::Dynamic(dynamic).representation(
                        db,
                        self.env,
                        self.settings.singleline(),
                    );
                    join.entry(&rep);
                }
                SubclassOfInner::Protocol(protocol) => {
                    let rep = Type::ProtocolInstance(protocol).representation(
                        db,
                        self.env,
                        self.settings.singleline(),
                    );
                    join.entry(&rep);
                }
                SubclassOfInner::TypeVar(bound_typevar) => {
                    let rep = Type::TypeVar(bound_typevar).representation(
                        db,
                        self.env,
                        self.settings.singleline(),
                    );
                    join.entry(&rep);
                }
            }
        }
        if !self.settings.preserve_full_unions {
            let omitted_entries = total_entries.saturating_sub(displayed_entries);
            if omitted_entries > 0 {
                join.entry(&DisplayOmitted {
                    count: omitted_entries,
                    singular: "type",
                    plural: "types",
                });
            }
        }
        join.finish()?;
        f.write_str("]")
    }
}

impl Display for DisplaySubclassOfGroup<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

struct DisplayLiteralGroup<'env, 'db> {
    literals: Vec<Type<'db>>,
    db: &'db dyn Db,
    env: &'env ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

const LITERAL_POLICY: TruncationPolicy = TruncationPolicy {
    max: 7,
    max_when_elided: 5,
};

impl<'db> FmtDetailed<'db> for DisplayLiteralGroup<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let db = self.db;
        f.with_type(Type::SpecialForm(SpecialFormType::Literal))
            .write_str("Literal")?;
        f.write_char('[')?;

        let total_entries = self.literals.len();

        let display_limit =
            LITERAL_POLICY.display_limit(total_entries, self.settings.preserve_full_unions);

        let mut join = f.join(", ");

        for lit in self.literals.iter().take(display_limit) {
            let rep = lit.representation(db, self.env, self.settings.singleline());
            join.entry(&rep);
        }

        if !self.settings.preserve_full_unions {
            let omitted_entries = total_entries.saturating_sub(display_limit);
            if omitted_entries > 0 {
                join.entry(&DisplayOmitted {
                    count: omitted_entries,
                    singular: "literal",
                    plural: "literals",
                });
            }
        }

        join.finish()?;
        f.write_str("]")
    }
}

impl Display for DisplayLiteralGroup<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

impl<'db> IntersectionType<'db> {
    fn display_with<'a>(
        &'a self,
        db: &'db dyn Db,
        env: &'a ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplayIntersectionType<'a, 'db> {
        DisplayIntersectionType {
            db,
            env,
            ty: self,
            settings,
        }
    }
}

struct DisplayIntersectionType<'a, 'db> {
    ty: &'a IntersectionType<'db>,
    db: &'db dyn Db,
    env: &'a ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> FmtDetailed<'db> for DisplayIntersectionType<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let db = self.db;
        let tys = self
            .ty
            .positive(db)
            .iter()
            .map(|&ty| DisplayMaybeNegatedType {
                ty,
                db,
                env: self.env,
                settings: self.settings.singleline(),
                negated: false,
            })
            .chain(
                self.ty
                    .negative(db)
                    .iter()
                    .map(|&ty| DisplayMaybeNegatedType {
                        ty,
                        db,
                        env: self.env,
                        settings: self.settings.singleline(),
                        negated: true,
                    }),
            );

        f.set_invalid_type_annotation();
        f.join(" & ").entries(tys).finish()
    }
}

impl Display for DisplayIntersectionType<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

impl fmt::Debug for DisplayIntersectionType<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, f)
    }
}

struct DisplayMaybeNegatedType<'env, 'db> {
    ty: Type<'db>,
    db: &'db dyn Db,
    env: &'env ProgramEnvironment<'db>,
    negated: bool,
    settings: DisplaySettings<'db>,
}

impl<'db> FmtDetailed<'db> for DisplayMaybeNegatedType<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let db = self.db;
        if self.negated {
            // basedpython renders negation as `not T`; standard typing-spec
            // display uses `~T`
            if basedpython_display_enabled() {
                f.write_str("not ")?;
            } else {
                f.write_str("~")?;
            }
        }
        DisplayMaybeParenthesizedType {
            ty: self.ty,
            db,
            env: self.env,
            settings: self.settings.clone(),
        }
        .fmt_detailed(f)
    }
}

impl Display for DisplayMaybeNegatedType<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

/// Returns `true` if the given type is a callable type that should be parenthesized
/// when appearing as a parameter annotation or return type of another callable.
///
/// Callable types with arrow syntax like `(int, str) -> bool` are parenthesized to
/// avoid ambiguity in nested callable displays. The exceptions are:
/// - Overloaded callables, which display as `Overload[...]` (already unambiguous)
/// - Callables with top-materialization parameters, which display as `Top[...]` (already unambiguous)
fn should_parenthesize_callable_type(ty: Type<'_>, db: &dyn Db) -> bool {
    if let Type::Callable(callable) = ty {
        let overloads = &callable.signatures(db).overloads;
        overloads.len() == 1 && !overloads[0].parameters().is_top()
    } else {
        false
    }
}

struct DisplayMaybeParenthesizedType<'env, 'db> {
    ty: Type<'db>,
    db: &'db dyn Db,
    env: &'env ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> FmtDetailed<'db> for DisplayMaybeParenthesizedType<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let db = self.db;
        let write_parentheses = |f: &mut TypeWriter<'_, '_, 'db>| {
            f.set_invalid_type_annotation();
            f.write_char('(')?;
            self.ty
                .display_with(db, self.env, self.settings.clone())
                .fmt_detailed(f)?;
            f.write_char(')')
        };
        match self.ty {
            ty if should_parenthesize_callable_type(ty, db) => write_parentheses(f),
            Type::KnownBoundMethod(_) | Type::FunctionLiteral(_) | Type::BoundMethod(_) => {
                write_parentheses(f)
            }
            Type::Union(union)
                if matches!(
                    self.settings.numeric_tower_display,
                    NumericTowerDisplay::Expanded
                ) || union.known(db).is_none() =>
            {
                write_parentheses(f)
            }
            Type::Intersection(intersection) if !intersection.has_one_element(db) => {
                write_parentheses(f)
            }
            _ => self
                .ty
                .display_with(db, self.env, self.settings.clone())
                .fmt_detailed(f),
        }
    }
}

impl Display for DisplayMaybeParenthesizedType<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

trait TypeArrayDisplay<'db> {
    fn display_with<'a>(
        &'a self,
        db: &'db dyn Db,
        env: &'a ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplayTypeArray<'a, 'db>;
}

impl<'db> TypeArrayDisplay<'db> for Box<[Type<'db>]> {
    fn display_with<'a>(
        &'a self,
        db: &'db dyn Db,
        env: &'a ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplayTypeArray<'a, 'db> {
        DisplayTypeArray {
            types: self,
            db,
            env,
            settings,
        }
    }
}

impl<'db> TypeArrayDisplay<'db> for Vec<Type<'db>> {
    fn display_with<'a>(
        &'a self,
        db: &'db dyn Db,
        env: &'a ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplayTypeArray<'a, 'db> {
        DisplayTypeArray {
            types: self,
            db,
            env,
            settings,
        }
    }
}

impl<'db> TypeArrayDisplay<'db> for [Type<'db>] {
    fn display_with<'a>(
        &'a self,
        db: &'db dyn Db,
        env: &'a ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplayTypeArray<'a, 'db> {
        DisplayTypeArray {
            types: self,
            db,
            env,
            settings,
        }
    }
}

struct DisplayTypeArray<'b, 'db> {
    types: &'b [Type<'db>],
    db: &'db dyn Db,
    env: &'b ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

impl<'db> FmtDetailed<'db> for DisplayTypeArray<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let db = self.db;
        f.join(", ")
            .entries(
                self.types
                    .iter()
                    .map(|ty| ty.display_with(db, self.env, self.settings.singleline())),
            )
            .finish()
    }
}

impl Display for DisplayTypeArray<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

impl<'db> StringLiteralType<'db> {
    fn display(self, db: &'db dyn Db) -> impl std::fmt::Display {
        std::fmt::from_fn(move |f| {
            f.write_char('"')?;
            for ch in self.value(db).chars() {
                match ch {
                    // `escape_debug` will escape even single quotes, which is not necessary for our
                    // use case as we are already using double quotes to wrap the string.
                    '\'' => f.write_char('\''),
                    _ => ch.escape_debug().fmt(f),
                }?;
            }
            f.write_char('"')
        })
    }
}

pub(crate) struct DisplayKnownInstanceRepr<'env, 'db> {
    known_instance: KnownInstanceType<'db>,
    db: &'db dyn Db,
    env: &'env ProgramEnvironment<'db>,
    settings: DisplaySettings<'db>,
}

/// If `ty` is a union that contains `None`, return the union of its remaining
/// (non-`None`) members; otherwise `None`. Used to render the innermost layer
/// of a wrapped optional (`int | None` -> base `int`) in `?` notation.
fn strip_none<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<Type<'db>> {
    let Type::Union(union) = ty else {
        return None;
    };
    let none = Type::none(db, env);
    let others: Vec<Type<'db>> = union
        .elements(db)
        .iter()
        .copied()
        .filter(|elem| *elem != none)
        .collect();
    if others.len() == union.elements(db).len() || others.is_empty() {
        return None;
    }
    Some(UnionType::from_elements(db, env, others))
}

impl<'db> KnownInstanceType<'db> {
    pub(crate) fn display_with<'env>(
        self,
        db: &'db dyn Db,
        env: &'env ProgramEnvironment<'db>,
        settings: DisplaySettings<'db>,
    ) -> DisplayKnownInstanceRepr<'env, 'db> {
        DisplayKnownInstanceRepr {
            known_instance: self,
            db,
            env,
            settings,
        }
    }
}

impl Display for DisplayKnownInstanceRepr<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_detailed(&mut TypeWriter::Formatter(f))
    }
}

impl<'db> FmtDetailed<'db> for DisplayKnownInstanceRepr<'_, 'db> {
    fn fmt_detailed(&self, f: &mut TypeWriter<'_, '_, 'db>) -> fmt::Result {
        let env = self.env;
        let db = self.db;
        let ty = Type::KnownInstance(self.known_instance);
        match self.known_instance {
            KnownInstanceType::SubscriptedProtocol(generic_context) => {
                f.set_invalid_type_annotation();
                f.write_str("<special-form '")?;
                f.with_type(Type::SpecialForm(SpecialFormType::Protocol))
                    .write_str("typing.Protocol")?;
                generic_context.display(db).fmt_detailed(f)?;
                f.write_str("'>")
            }
            KnownInstanceType::SubscriptedGeneric(generic_context) => {
                f.set_invalid_type_annotation();
                f.write_str("<special-form '")?;
                f.with_type(Type::SpecialForm(SpecialFormType::Generic))
                    .write_str("typing.Generic")?;
                generic_context.display(db).fmt_detailed(f)?;
                f.write_str("'>")
            }
            KnownInstanceType::TypeAliasType(alias) => {
                if let Some(specialization) = alias.specialization(db) {
                    f.set_invalid_type_annotation();
                    f.write_str("<type alias '")?;
                    f.with_type(ty).write_str(alias.name(db))?;
                    specialization
                        .display_short(
                            db,
                            self.env,
                            TupleSpecialization::No,
                            DisplaySettings::default(),
                        )
                        .fmt_detailed(f)?;
                    f.write_str("'>")
                } else {
                    f.with_type(ty).write_str("TypeAliasType")
                }
            }
            // This is a legacy `TypeVar` _outside_ of any generic class or function, so we render
            // it as an instance of `typing.TypeVar`. Inside of a generic class or function, we'll
            // have a `Type::TypeVar(_)`, which is rendered as the typevar's name.
            KnownInstanceType::TypeVar(typevar_instance) => {
                // basedpython spells a keyword-variadic pack the way python spells a
                // `ParamSpec`, and python builds a real `ParamSpec` object for it — but
                // it is specialized by keyword and forwards no parameter list, so naming
                // it after the runtime object would describe none of what it does
                if typevar_instance.kind(self.db).is_keyword_variadic() {
                    f.with_type(ty).write_str("KeywordPack")
                } else if typevar_instance.kind(self.db).is_parameter_pack() {
                    f.with_type(ty).write_str("ParamSpec")
                } else if typevar_instance.kind(db).is_typevartuple() {
                    f.with_type(ty).write_str("TypeVarTuple")
                } else {
                    f.with_type(ty).write_str("TypeVar")
                }
            }
            KnownInstanceType::Deprecated(_) => f.write_str("warnings.deprecated"),
            KnownInstanceType::Field(field) => {
                f.with_type(ty).write_str("dataclasses.Field")?;

                let field_type = field
                    .converter(db)
                    .map(|(_, converter_output)| converter_output)
                    .or(field.default_type(db));

                if let Some(field_ty) = field_type {
                    f.write_char('[')?;
                    write!(f.with_type(field_ty), "{}", field_ty.display(db, self.env))?;
                    f.write_char(']')?;
                }
                Ok(())
            }
            KnownInstanceType::ConstraintSet(interned_set) => {
                f.with_type(ty).write_str("ConstraintSet")?;
                let constraints = ConstraintSetBuilder::new();
                let set = constraints.load(db, self.env, interned_set.constraints(db));
                if interned_set.detailed_display(db) {
                    write!(f, "[{}]", set.display(db, self.env))
                } else if set.is_always_satisfied(db, self.env) {
                    f.write_str("[Literal[True]]")
                } else if set.is_never_satisfied(db, self.env) {
                    f.write_str("[Literal[False]]")
                } else {
                    f.write_str("[bool]")
                }
            }
            KnownInstanceType::ConstraintSetSolution(solution) => {
                f.set_invalid_type_annotation();
                f.with_type(ty).write_str("Solution[")?;
                for (index, binding) in solution.bindings(db).iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}=", binding.bound_typevar.name(db))?;
                    binding
                        .solution
                        .display_with(db, self.env, self.settings.clone())
                        .fmt_detailed(f)?;
                }
                f.write_char(']')
            }
            KnownInstanceType::GenericContext(generic_context) => {
                f.with_type(ty)
                    .write_str("ty_extensions._internal.GenericContext")?;
                write!(f, "{}", generic_context.display_full(db))
            }
            KnownInstanceType::Specialization(specialization) => {
                // Normalize for consistent output across CI platforms
                f.with_type(ty)
                    .write_str("ty_extensions._internal.Specialization")?;
                write!(f, "{}", specialization.display_full(db, self.env))
            }
            KnownInstanceType::UnionType(union) => {
                f.set_invalid_type_annotation();
                f.write_char('<')?;
                f.with_type(KnownClass::UnionType.to_class_literal(db, self.env))
                    .write_str("types.UnionType")?;
                f.write_str(" special-form")?;
                if let Ok(ty) = union.union_type(db) {
                    f.write_str(" '")?;
                    ty.display(db, self.env).fmt_detailed(f)?;
                    f.write_char('\'')?;
                }
                f.write_char('>')
            }
            KnownInstanceType::Literal(inner) => {
                f.set_invalid_type_annotation();
                f.write_str("<special-form '")?;
                inner.inner(db).display(db, self.env).fmt_detailed(f)?;
                f.write_str("'>")
            }
            KnownInstanceType::Annotated(inner) => {
                f.set_invalid_type_annotation();
                f.write_str("<special-form '")?;
                f.with_type(Type::SpecialForm(SpecialFormType::Annotated))
                    .write_str("typing.Annotated")?;
                f.write_char('[')?;
                inner.inner(db).display(db, self.env).fmt_detailed(f)?;
                f.write_str(", <metadata>]'>")
            }
            KnownInstanceType::WrappedOptional(inner) => {
                // render a nested optional in surface `?` notation: count the
                // wrapper layers, then the innermost `base | None` union adds
                // one more `?`. e.g. `WrappedOptional(int | None)` -> `int??`
                let mut depth = 1usize;
                let mut current = inner.inner(self.db);
                while let Type::KnownInstance(KnownInstanceType::WrappedOptional(next)) = current {
                    depth += 1;
                    current = next.inner(self.db);
                }
                let (base, extra) = match strip_none(self.db, env, current) {
                    Some(base) => (base, 1),
                    None => (current, 0),
                };
                let parenthesize = matches!(base, Type::Union(_) | Type::Intersection(_));
                if parenthesize {
                    f.write_char('(')?;
                }
                base.display(self.db, env).fmt_detailed(f)?;
                if parenthesize {
                    f.write_char(')')?;
                }
                for _ in 0..(depth + extra) {
                    f.write_char('?')?;
                }
                Ok(())
            }
            KnownInstanceType::Callable(callable) => {
                f.set_invalid_type_annotation();
                f.write_char('<')?;
                // Ensure that when we go-to-definition on an inlay hint for a `Callable`,
                // regardless of whether it's imported from `collections.abc` or `typing`,
                // we go to `typing.pyi` because in typeshed there is no `Callable` in
                // `collections.abc`.
                f.with_type(Type::SpecialForm(SpecialFormType::TypingCallable))
                    .write_str("Callable")?;
                f.write_str(" special-form '")?;
                callable
                    .display_with(db, self.env, self.settings.clone())
                    .fmt_detailed(f)?;
                f.write_str("'>")
            }
            KnownInstanceType::TypeGenericAlias(inner) => {
                f.set_invalid_type_annotation();
                f.write_str("<special-form '")?;
                f.with_type(KnownClass::Type.to_class_literal(db, self.env))
                    .write_str("type")?;
                f.write_char('[')?;
                inner.inner(db).display(db, self.env).fmt_detailed(f)?;
                f.write_str("]'>")
            }
            KnownInstanceType::LiteralStringAlias(_) => f
                .with_type(KnownClass::Str.to_class_literal(db, self.env))
                .write_str("str"),
            KnownInstanceType::NewType(declaration) => {
                f.set_invalid_type_annotation();
                f.write_char('<')?;
                f.with_type(KnownClass::NewType.to_class_literal(db, self.env))
                    .write_str("NewType")?;
                f.write_str(" pseudo-class '")?;
                f.with_type(ty).write_str(declaration.name(db))?;
                f.write_str("'>")
            }
            KnownInstanceType::Sentinel(sentinel) => {
                f.with_type(ty).write_str(sentinel.name(db).as_str())
            }
            KnownInstanceType::NamedTupleSpec(_) => f.write_str("NamedTupleSpec"),
            KnownInstanceType::FunctoolsPartial(partial) => {
                f.write_str("partial[")?;
                Type::Callable(partial.partial(db))
                    .display_with(db, self.env, DisplaySettings::default().singleline())
                    .fmt_detailed(f)?;
                f.write_str("]")
            }
            KnownInstanceType::Range { .. } => f
                .with_type(KnownClass::Range.to_class_literal(db, self.env))
                .write_str("range"),
            KnownInstanceType::FunctoolsPartialCall(partial) => Type::Callable(partial.partial(db))
                .display_with(db, self.env, DisplaySettings::default().singleline())
                .fmt_detailed(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use ruff_python_ast::name::Name;

    use crate::db::tests::{TestDb, setup_db};
    use crate::types::{
        KnownClass, KnownUnion, Parameter, Parameters, Signature, Type, TypeDetail, UnionType,
    };

    #[test]
    fn string_literal_display() {
        let db = setup_db();

        assert_eq!(
            Type::string_literal(&db, r"\n")
                .display(&db, &db.program_environment())
                .to_string(),
            r#"Literal["\\n"]"#
        );
        assert_eq!(
            Type::string_literal(&db, "'")
                .display(&db, &db.program_environment())
                .to_string(),
            r#"Literal["'"]"#
        );
        assert_eq!(
            Type::string_literal(&db, r#"""#)
                .display(&db, &db.program_environment())
                .to_string(),
            r#"Literal["\""]"#
        );
    }

    #[test]
    fn numeric_tower_display() {
        let db = setup_db();
        let env = db.program_environment();

        let exact_float = KnownClass::Float.to_instance(&db, &env);
        let exact_complex = KnownClass::Complex.to_instance(&db, &env);
        let float_annotation = KnownUnion::Float.to_type(&db, &env);
        let complex_annotation = KnownUnion::Complex.to_type(&db, &env);

        // a type is shown as what it is: the exact class is its own name, and the
        // promoted annotation is the union it stands for. nothing is collapsed into a
        // narrower-reading spelling, and nothing is marked with a `*`
        assert_snapshot!(exact_float.display(&db, &env), @"float");
        assert_snapshot!(exact_complex.display(&db, &env), @"complex");
        assert_snapshot!(float_annotation.display(&db, &env), @"int | float");
        assert_snapshot!(complex_annotation.display(&db, &env), @"int | float | complex");
        assert_snapshot!(float_annotation.to_meta_type(&db, &env).display(&db, &env), @"type[int | float]");
        assert_snapshot!(complex_annotation.to_meta_type(&db, &env).display(&db, &env), @"type[int | float | complex]");

        let list_of_float =
            KnownClass::List.to_specialized_instance(&db, &env, &[float_annotation]);
        assert_snapshot!(list_of_float.display(&db, &env), @"list[int | float]");

        let string_or_float = UnionType::from_elements(
            &db,
            &env,
            [KnownClass::Str.to_instance(&db, &env), float_annotation],
        );
        assert_snapshot!(string_or_float.display(&db, &env), @"str | int | float");

        // both spellings are ordinary type syntax now — neither needs a marker
        assert!(
            exact_float
                .display(&db, &env)
                .to_string_parts()
                .is_valid_syntax
        );
        assert!(
            float_annotation
                .display(&db, &env)
                .to_string_parts()
                .is_valid_syntax
        );
        // the annotation renders as the union it stands for, so both members are
        // navigable rather than only the one the collapsed spelling named
        assert!(matches!(
            float_annotation
                .display(&db, &env)
                .to_string_parts()
                .details
                .as_slice(),
            [
                TypeDetail::Type(Type::ClassLiteral(int)),
                TypeDetail::Type(Type::ClassLiteral(float)),
            ] if int.known(&db) == Some(KnownClass::Int)
                && float.known(&db) == Some(KnownClass::Float)
        ));
    }

    fn display_signature<'db>(
        db: &'db TestDb,
        parameters: impl IntoIterator<Item = Parameter<'db>>,
        return_ty: Option<Type<'db>>,
    ) -> String {
        Signature::new(
            Parameters::from_annotation(db, &db.program_environment(), parameters),
            return_ty.unwrap_or(Type::unknown()),
        )
        .display(db, &db.program_environment())
        .to_string()
    }

    fn display_signature_multiline<'db>(
        db: &'db TestDb,
        parameters: impl IntoIterator<Item = Parameter<'db>>,
        return_ty: Option<Type<'db>>,
    ) -> String {
        Signature::new(
            Parameters::from_annotation(db, &db.program_environment(), parameters),
            return_ty.unwrap_or(Type::unknown()),
        )
        .display_with(
            db,
            &db.program_environment(),
            super::DisplaySettings::default().multiline(),
        )
        .to_string()
    }

    #[test]
    fn signature_display() {
        let db = setup_db();
        let db = &db;
        let env = db.program_environment();

        // Empty parameters with no return type.
        assert_snapshot!(display_signature(db, [], None), @"() -> Unknown");

        // Empty parameters with a return type.
        assert_snapshot!(
            display_signature(db, [], Some(Type::none(db, &env))),
            @"() -> None"
        );

        // Single parameter type (no name) with a return type.
        assert_snapshot!(
            display_signature(
                db,
                [Parameter::positional_only(None).with_annotated_type(Type::none(db, &env))],
                Some(Type::none(db, &env))
            ),
            @"(None, /) -> None"
        );

        // Two parameters where one has annotation and the other doesn't.
        assert_snapshot!(
            display_signature(
                db,
                [
                    Parameter::positional_or_keyword(Name::new_static("x"))
                        .with_default_type(KnownClass::Int.to_instance(db, &env)),
                    Parameter::positional_or_keyword(Name::new_static("y"))
                        .with_annotated_type(KnownClass::Str.to_instance(db, &env))
                        .with_default_type(KnownClass::Str.to_instance(db, &env)),
                ],
                Some(Type::none(db, &env))
            ),
            @"(x=..., y: str = ...) -> None"
        );

        // All positional only parameters.
        assert_snapshot!(
            display_signature(
                db,
                [
                    Parameter::positional_only(Some(Name::new_static("x"))),
                    Parameter::positional_only(Some(Name::new_static("y"))),
                ],
                Some(Type::none(db, &env))
            ),
            @"(x, y, /) -> None"
        );

        // Positional-only parameters mixed with non-positional-only parameters.
        assert_snapshot!(
            display_signature(
                db,
                [
                    Parameter::positional_only(Some(Name::new_static("x"))),
                    Parameter::positional_or_keyword(Name::new_static("y")),
                ],
                Some(Type::none(db, &env))
            ),
            @"(x, /, y) -> None"
        );

        // All keyword-only parameters.
        assert_snapshot!(
            display_signature(
                db,
                [
                    Parameter::keyword_only(Name::new_static("x")),
                    Parameter::keyword_only(Name::new_static("y")),
                ],
                Some(Type::none(db, &env))
            ),
            @"(*, x, y) -> None"
        );

        // Keyword-only parameters mixed with non-keyword-only parameters.
        assert_snapshot!(
            display_signature(
                db,
                [
                    Parameter::positional_or_keyword(Name::new_static("x")),
                    Parameter::keyword_only(Name::new_static("y")),
                ],
                Some(Type::none(db, &env))
            ),
            @"(x, *, y) -> None"
        );

        // '/' parameter must appear before '*' parameter
        assert_snapshot!(
            display_signature(
                db,
                [
                    Parameter::positional_only(Some(Name::new_static("a"))),
                    Parameter::keyword_only(Name::new_static("x")),
                    Parameter::keyword_only(Name::new_static("y")),
                ],
                Some(Type::none(db, &env))
            ),
            @"(a, /, *, x, y) -> None"
        );

        // A mix of all parameter kinds.
        assert_snapshot!(
            display_signature(
                db,
                [
                    Parameter::positional_only(Some(Name::new_static("a"))),
                    Parameter::positional_only(Some(Name::new_static("b")))
                        .with_annotated_type(KnownClass::Int.to_instance(db, &env)),
                    Parameter::positional_only(Some(Name::new_static("c")))
                        .with_default_type(Type::int_literal(1)),
                    Parameter::positional_only(Some(Name::new_static("d")))
                        .with_annotated_type(KnownClass::Int.to_instance(db, &env))
                        .with_default_type(Type::int_literal(2)),
                    Parameter::positional_or_keyword(Name::new_static("e"))
                        .with_default_type(Type::int_literal(3)),
                    Parameter::positional_or_keyword(Name::new_static("f"))
                        .with_annotated_type(KnownClass::Int.to_instance(db, &env))
                        .with_default_type(Type::int_literal(4)),
                    Parameter::variadic(Name::new_static("args"))
                        .with_annotated_type(Type::object()),
                    Parameter::keyword_only(Name::new_static("g"))
                        .with_default_type(Type::int_literal(5)),
                    Parameter::keyword_only(Name::new_static("h"))
                        .with_annotated_type(KnownClass::Int.to_instance(db, &env))
                        .with_default_type(Type::int_literal(6)),
                    Parameter::keyword_variadic(Name::new_static("kwargs"))
                        .with_annotated_type(KnownClass::Str.to_instance(db, &env)),
                ],
                Some(KnownClass::Bytes.to_instance(db, &env))
            ),
            @"(a, b: int, c=1, d: int = 2, /, e=3, f: int = 4, *args: object, g=5, h: int = 6, **kwargs: str) -> bytes"
        );
    }

    #[test]
    fn signature_display_multiline() {
        let db = setup_db();
        let db = &db;
        let env = db.program_environment();

        // Empty parameters with no return type.
        assert_snapshot!(display_signature_multiline(db, [], None), @"() -> Unknown");

        // Empty parameters with a return type.
        assert_snapshot!(
            display_signature_multiline(db, [], Some(Type::none(db, &env))),
            @"() -> None"
        );

        // Single parameter type (no name) with a return type.
        assert_snapshot!(
            display_signature_multiline(
                db,
                [Parameter::positional_only(None).with_annotated_type(Type::none(db, &env))],
                Some(Type::none(db, &env))
            ),
            @"(None, /) -> None"
        );

        // Two parameters where one has annotation and the other doesn't.
        assert_snapshot!(
            display_signature_multiline(
                db,
                [
                    Parameter::positional_or_keyword(Name::new_static("x"))
                        .with_default_type(KnownClass::Int.to_instance(db, &env)),
                    Parameter::positional_or_keyword(Name::new_static("y"))
                        .with_annotated_type(KnownClass::Str.to_instance(db, &env))
                        .with_default_type(KnownClass::Str.to_instance(db, &env)),
                ],
                Some(Type::none(db, &env))
            ),
            @"
        (
            x=...,
            y: str = ...
        ) -> None
        "
        );

        // All positional only parameters.
        assert_snapshot!(
            display_signature_multiline(
                db,
                [
                    Parameter::positional_only(Some(Name::new_static("x"))),
                    Parameter::positional_only(Some(Name::new_static("y"))),
                ],
                Some(Type::none(db, &env))
            ),
            @"
        (
            x,
            y,
            /
        ) -> None
        "
        );

        // Positional-only parameters mixed with non-positional-only parameters.
        assert_snapshot!(
            display_signature_multiline(
                db,
                [
                    Parameter::positional_only(Some(Name::new_static("x"))),
                    Parameter::positional_or_keyword(Name::new_static("y")),
                ],
                Some(Type::none(db, &env))
            ),
            @"
        (
            x,
            /,
            y
        ) -> None
        "
        );

        // All keyword-only parameters.
        assert_snapshot!(
            display_signature_multiline(
                db,
                [
                    Parameter::keyword_only(Name::new_static("x")),
                    Parameter::keyword_only(Name::new_static("y")),
                ],
                Some(Type::none(db, &env))
            ),
            @"
        (
            *,
            x,
            y
        ) -> None
        "
        );

        // Keyword-only parameters mixed with non-keyword-only parameters.
        assert_snapshot!(
            display_signature_multiline(
                db,
                [
                    Parameter::positional_or_keyword(Name::new_static("x")),
                    Parameter::keyword_only(Name::new_static("y")),
                ],
                Some(Type::none(db, &env))
            ),
            @"
        (
            x,
            *,
            y
        ) -> None
        "
        );

        // A mix of all parameter kinds.
        assert_snapshot!(
            display_signature_multiline(
                db,
                [
                    Parameter::positional_only(Some(Name::new_static("a"))),
                    Parameter::positional_only(Some(Name::new_static("b")))
                        .with_annotated_type(KnownClass::Int.to_instance(db, &env)),
                    Parameter::positional_only(Some(Name::new_static("c")))
                        .with_default_type(Type::int_literal(1)),
                    Parameter::positional_only(Some(Name::new_static("d")))
                        .with_annotated_type(KnownClass::Int.to_instance(db, &env))
                        .with_default_type(Type::int_literal(2)),
                    Parameter::positional_or_keyword(Name::new_static("e"))
                        .with_default_type(Type::int_literal(3)),
                    Parameter::positional_or_keyword(Name::new_static("f"))
                        .with_annotated_type(KnownClass::Int.to_instance(db, &env))
                        .with_default_type(Type::int_literal(4)),
                    Parameter::variadic(Name::new_static("args"))
                        .with_annotated_type(Type::object()),
                    Parameter::keyword_only(Name::new_static("g"))
                        .with_default_type(Type::int_literal(5)),
                    Parameter::keyword_only(Name::new_static("h"))
                        .with_annotated_type(KnownClass::Int.to_instance(db, &env))
                        .with_default_type(Type::int_literal(6)),
                    Parameter::keyword_variadic(Name::new_static("kwargs"))
                        .with_annotated_type(KnownClass::Str.to_instance(db, &env)),
                ],
                Some(KnownClass::Bytes.to_instance(db, &env))
            ),
            @"
        (
            a,
            b: int,
            c=1,
            d: int = 2,
            /,
            e=3,
            f: int = 4,
            *args: object,
            g=5,
            h: int = 6,
            **kwargs: str
        ) -> bytes
        "
        );
    }
}
