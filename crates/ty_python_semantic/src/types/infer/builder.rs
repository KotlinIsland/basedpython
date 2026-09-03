use std::cell::{OnceCell, RefCell};
use std::collections::hash_map;
use std::rc::Rc;

use compact_str::CompactString;
use itertools::Itertools;
use ruff_db::diagnostic::{Annotation, Diagnostic, Span};
use ruff_db::files::File;
use ruff_db::parsed::ParsedModuleRef;
use ruff_db::source::source_text;
use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::helpers::{
    BindingKeyword, TypeModifier, is_declaration_marker, is_dotted_name,
    is_untyped_declaration_marker, untyped_declaration_context,
};
use ruff_python_ast::name::Name;
use ruff_python_ast::{
    self as ast, AnyNodeRef, ArgOrKeyword, ArgumentsSourceOrder, ExprContext, HasNodeIndex,
    PySourceType, PythonVersion,
};
use ruff_python_stdlib::builtins::version_builtin_was_added;
use ruff_python_stdlib::typing::as_pep_585_generic;
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use strum::IntoEnumIterator;
use ty_module_resolver::{ImportingFile, KnownModule, ModuleName, file_to_module, resolve_module};
use ty_python_core::ast_ids::HasScopedUseId;
use ty_python_core::statement::StatementInner;

use super::{
    CollectionUseConstraints, DeferredAndUndecorated, DefinitionInference,
    DefinitionInferenceExtra, DefinitionTypes, ExpressionInference, ExpressionInferenceExtra,
    FrozenMap, FrozenSet, FrozenValueMap, FunctionDecoratorInference, InferenceRegion,
    OtherDefinitionInferenceExtra, ScopeInference, ScopeInferenceExtra, function_known_decorators,
    infer_deferred_types, infer_definition_types, infer_expression_types,
    infer_same_file_expression_type, infer_unpack_types,
};
use crate::diagnostic::format_enumeration;
use crate::place::{
    ConsideredDefinitions, DefinedPlace, Definedness, LookupError, Place, PlaceAndQualifiers,
    RequiresExplicitReExport, TypeOrigin, builtins_module_scope, class_body_implicit_symbol,
    declared_type_at_load, explicit_global_symbol, implicit_builtins_symbol,
    loop_header_reachability, module_type_implicit_global_declaration,
    module_type_implicit_global_symbol, place_by_id, place_from_bindings_with_reachability_cache,
    place_from_declarations_with_reachability_cache, typing_extensions_symbol,
};
use crate::place_load::{
    ImplicitPlaceLoad, PlaceExprPrefixLoad, PlaceExprPrefixLoads, PlaceLoadFailure, PlaceLoadMode,
    PlaceLoadResolutionStep, PlaceLoadSource, PlaceLoadSourceKind, resolve_place_load,
};
use crate::reachability::{
    ReachabilityEvaluationCache, analyze_pattern_predicate, evaluate_reachability,
    evaluate_reachability_with_cache, is_reachable,
};
use crate::subscript::PyIndex;
use crate::types::add_inferred_python_version_hint_to_diagnostic;
use crate::types::attribute_write::{AssignmentAttributeMembers, assignment_attribute_members};
use crate::types::call::bind::{
    ArgumentTypeContext, CheckTypesMode, OverloadSet, requires_overload_evaluation,
};
use crate::types::call::{Argument, Binding, Bindings, CallArguments, CallError, CallErrorKind};
use crate::types::callable::{CallableFunctionProvenance, CallableTypeKind};
use crate::types::class::{
    ClassLiteral, CodeGeneratorKind, DynamicNamedTupleAnchor, DynamicNamedTupleLiteral,
    DynamicTypedDictAnchor, DynamicTypedDictLiteral, FrozenDataclassDispatch, MethodDecorator,
    NamedTupleField, NamedTupleSpec, StaticClassLiteral,
};
use crate::types::constraints::{ConstraintSetBuilder, PathBounds, Solutions};
use crate::types::context::InferContext;
use crate::types::context_sensitive::{self, case_name_pattern_type};
use crate::types::dedicated::{django, pydantic};
use crate::types::deferred::{is_integer_operand, is_symbolic_operand};
use crate::types::diagnostic::{
    self, AMBIGUOUS_EXTENSION_MEMBER, CALL_NON_CALLABLE, CONFLICTING_DECLARATIONS,
    CYCLIC_TYPE_ALIAS_DEFINITION, ERASED_CAST_ARGUMENT, ERASED_TYPE_CHECK, FINAL_ON_VARIABLE,
    GeneratorMismatchKind, IMPLICIT_DECLARATION, INEFFECTIVE_FINAL, INVALID_ARGUMENT_TYPE,
    INVALID_ASSIGNMENT, INVALID_DECLARATION, INVALID_ENUM_MEMBER_ANNOTATION, INVALID_FIELD_LOOKUP,
    INVALID_LEGACY_TYPE_VARIABLE, INVALID_NEWTYPE, INVALID_PARAMSPEC, INVALID_REGEX,
    INVALID_REIFIED_TYPE_PARAM, INVALID_TYPE_ALIAS_TYPE, INVALID_TYPE_FORM,
    INVALID_TYPE_VARIABLE_CONSTRAINTS, INVALID_TYPE_VARIABLE_DEFAULT, INVALID_VARIANCE_DECLARATION,
    NARROWING_GUARD_AS_VALUE, NON_EXHAUSTIVE_STATEMENT_EXPRESSION, NON_OVERLAPPING_CAST,
    NON_OVERLAPPING_TYPE_TEST, OPTIONAL_OBJECT_CONVERSION, POSSIBLY_MISSING_IMPLICIT_CALL,
    POSSIBLY_MISSING_SUBMODULE, REFUTABLE_DESTRUCTURING, REFUTABLE_UNPACKING,
    TRAILING_LAMBDA_PARAMETERS, TypeCheckDiagnostics, UNANNOTATED_MODEL_FIELD,
    UNAVAILABLE_IMPLICIT_SUPER_ARGUMENTS, UNDEFINED_REVEAL, UNRESOLVED_ATTRIBUTE,
    UNRESOLVED_GLOBAL, UNRESOLVED_REFERENCE, UNSOUND_CAST, UNSOUND_YIELD,
    UNSPECIALIZED_REIFIED_GENERIC, UNSUPPORTED_OPERATOR, UNUSED_AWAITABLE, YieldKind,
    display_required_elements, hint_if_stdlib_attribute_exists_on_other_versions,
    refutable_unpacking_applies, report_attempted_protocol_instantiation,
    report_bad_dunder_delattr_call, report_bad_dunder_delete_call, report_bool_as_int,
    report_bool_as_int_assignment, report_call_to_abstract_method,
    report_cannot_pop_required_field_on_typed_dict, report_capturing_case_name,
    report_capturing_case_name_alternative, report_invalid_assignment,
    report_invalid_class_match_pattern, report_invalid_exception_caught,
    report_invalid_exception_cause, report_invalid_exception_raised,
    report_invalid_exception_tuple_caught, report_invalid_generator_yield_type,
    report_invalid_key_on_typed_dict, report_invalid_match_args_type,
    report_invalid_type_checking_constant,
    report_match_pattern_against_non_runtime_checkable_protocol,
    report_match_pattern_against_typed_dict, report_mismatched_type_name,
    report_possibly_missing_attribute, report_possibly_unresolved_reference,
    report_too_many_positional_patterns_for_class_pattern, report_unsound_yield,
    report_unsupported_augmented_assignment, report_unsupported_comparison,
};
use crate::types::enums::{enum_ignored_names, is_enum_class_by_inheritance};
use crate::types::extensions;
use crate::types::format;
use crate::types::function::{
    FunctionDecorators, FunctionType, KnownFunction, report_revealed_type,
    same_module_uncached_raw_signature,
};
use crate::types::generics::{
    GenericContext, Specialization, SpecializationBuilder, bind_typevar, enclosing_binding_contexts,
};
use crate::types::implicit_names::implicit_name;
use crate::types::infer::builder::named_tuple::NamedTupleKind;
use crate::types::infer::builder::paramspec_validation::validate_paramspec_components;
use crate::types::infer::{
    StatementInference, StatementInferenceInner, StatementInferenceInnerExtra, TypeAndRange,
    TypeExpressionFlags, infer_statement_types, nearest_enclosing_class,
    nearest_enclosing_function, original_class_type,
};
use crate::types::match_pattern::{ClassPatternPositionalResult, class_pattern_positional_result};
use crate::types::match_type::literal_pattern_type;
use crate::types::narrow::NarrowingEvaluatorExtension;
use crate::types::narrow::{pattern_subject_type, pattern_success_types};
use crate::types::newtype::NewType;
use crate::types::receivers;
use crate::types::regex;
use crate::types::reified_infer::{self, ErasedTargetReason, ReifiedInferenceError};
use crate::types::set_theoretic::RecursivelyDefined;
use crate::types::signatures::{CallableSignature, NarrowingGuard, ReturnCallableTypeVarScope};
use crate::types::soundness::{
    cast_is_redundant, cast_target_is_unverifiable_protocol, erases_type_arguments,
    runtime_check_target,
};
use crate::types::special_form::TypeQualifier;
use crate::types::subclass_of::SubclassOfInner;
use crate::types::template::{Promotable, TemplateLiteralType, TemplatePart};
use crate::types::trailing_lambda::{
    enclosing_block, trailing_lambda_keyword, trailing_lambda_passes_it,
};
use crate::types::tuple::promotion::TupleSizePromotionConstraints;
use crate::types::tuple::{Tuple, TupleLength, TupleSpecBuilder, TupleType, VariableSegment};
use crate::types::type_alias::{ManualPEP695TypeAliasType, PEP695TypeAliasType};
use crate::types::typed_dict::{TypedDictAssignmentKind, TypedDictKeyAssignment};
use crate::types::typevar::{
    BoundTypeVarIdentity, TypeVarConstraints, TypeVarIdentity, TypeVarInstance, TypeVarSet,
};
use crate::types::unpacker::UnpackResult;
use crate::types::{
    BindingContext, BoundTypeVarInstance, CallDunderError, CallableBinding, CallableType,
    CallableTypes, ClassType, DeferredOperation, DeferredType, DynamicType, InferenceFlags,
    InstanceProjection, InternedConstraintSet, InternedType, IntersectionBuilder, IntersectionType,
    KnownClass, KnownInstanceType, KnownUnion, LiteralValueType, LiteralValueTypeKind,
    MemberLookupPolicy, ParamSpecAttrKind, Parameter, Parameters, ProgramEnvironment,
    RestrictedType, SentinelInstance, Signature, SpecialFormType, SubclassOfType, Type,
    TypeAliasType, TypeAndQualifiers, TypeContext, TypeQualifiers, TypeVarBoundOrConstraints,
    TypeVarKind, TypeVarVariance, TypedDictModule, TypedDictType, UnionAccumulator, UnionBuilder,
    UnionType, any_over_type, binding_type, extract_fixed_length_iterable_element_types,
    infer_complete_scope_types, infer_scope_types, is_discarded_dict_key_assignment,
    report_iteration_over_character, todo_type,
};
use crate::{AnalysisSettings, Db, FxIndexSet, FxOrderSet};
use fluid::FluidTimeline;
use ty_python_core::BlockScopedDeclaration;
use ty_python_core::definition::{
    AnnotatedAssignmentDefinitionKind, AssignmentDefinitionKind, ComprehensionDefinitionKind,
    Definition, DefinitionKind, DefinitionNodeKey, DefinitionState, ExceptHandlerDefinitionKind,
    ForStmtDefinitionKind, LambdaParameterDefinitionNodeKind, LoopHeaderDefinitionKind,
    NestedBindingExecution, NestedBindingsDefinitionKind, ParameterDefinitionNodeKind, TargetKind,
    WithItemDefinitionKind,
};
use ty_python_core::expression::{Expression, ExpressionKind};
use ty_python_core::narrowing_constraints::ConstraintKey;
use ty_python_core::node_key::NodeKey;
use ty_python_core::place::{PlaceExpr, PlaceExprRef};
use ty_python_core::predicate::PatternPredicate;
use ty_python_core::scope::{FileScopeId, NodeWithScopeKind, NodeWithScopeRef, ScopeId, ScopeKind};
use ty_python_core::symbol::ScopedSymbolId;
use ty_python_core::{
    ApplicableConstraints, EvaluationMode, ProgramFile, SemanticIndex, Truthiness,
    unpack::UnpackPosition,
};
use ty_python_core::{ExpressionNodeKey, Statement};

mod annotation_expression;
mod attribute_assignment;
mod binary_expressions;
pub(crate) use binary_expressions::{
    fold_tuple_concat, fold_tuple_repeat, literal_binary_op, literal_unary_op,
};
mod class;
mod conditions;
mod dict;
mod dynamic_class;
mod enum_call;
mod final_attribute;
pub(super) mod fluid;
mod function;
mod imports;
mod named_tuple;
mod new_class;
mod paramspec_validation;
mod post_inference;
mod return_value_use;
mod subscript;
mod type_call;
mod type_expression;
mod type_form;
mod typed_dict;
mod typeguard;
mod typevar;

use super::comparisons;

/// A helper to track if we already know that declared and inferred types are the same.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DeclaredAndInferredType<'db> {
    /// We know that both the declared and inferred types are the same.
    AreTheSame(TypeAndQualifiers<'db>),
    /// Declared and inferred types might be different, we need to check assignability.
    MightBeDifferent {
        declared_ty: TypeAndQualifiers<'db>,
        inferred_ty: Type<'db>,
    },
}

impl<'db> DeclaredAndInferredType<'db> {
    fn are_the_same_type(ty: Type<'db>) -> Self {
        Self::AreTheSame(TypeAndQualifiers::new(
            ty,
            TypeOrigin::Inferred,
            TypeQualifiers::empty(),
        ))
    }
}

fn should_preserve_inferred_binding_type<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    // Dataclass field specifiers carry metadata in the inferred RHS type; replacing it with the
    // declared field type would lose settings like `init=False`.
    matches!(ty, Type::KnownInstance(KnownInstanceType::Field(_)))
        // A pattern's statically-known capture groups ride on the inferred type
        // in the same way, and `p: re.Pattern[str] = re.compile("()")` would
        // otherwise throw them away.
        || regex::groups_of(db, ty).is_some()
}

/// Whether `ty` carries an optional layer at its top level: a wrapped optional
/// (`int??`), or a union with a `None` arm alongside at least one other member
/// (`int | None`). A bare `None` is not optional — there is nothing to lose.
fn is_optional_value<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    match ty {
        Type::KnownInstance(KnownInstanceType::WrappedOptional(_)) => true,
        Type::Union(union) => {
            let elements = union.elements(db);
            elements.iter().any(|element| element.is_none(db))
                && elements.iter().any(|element| !element.is_none(db))
        }
        _ => false,
    }
}

/// Whether `ty` is the top type `object` (or `object | None`, the `object?`
/// surface form) — a target that absorbs an optional's `None` arm without
/// preserving it. Only such a target makes the widening both silent and lossy.
fn target_swallows_optional<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> bool {
    match ty {
        Type::NominalInstance(instance) => instance
            .class(db, env)
            .class_literal(db)
            .is_known(db, KnownClass::Object),
        Type::Union(union) => {
            let elements = union.elements(db);
            elements
                .iter()
                .any(|element| target_swallows_optional(db, env, *element))
                && elements.iter().all(|element| {
                    element.is_none(db) || target_swallows_optional(db, env, *element)
                })
        }
        _ => false,
    }
}

/// We currently store one dataclass field-specifiers inline, because that covers standard
/// dataclasses. attrs uses 2 specifiers, pydantic and strawberry use 3 specifiers. SQLAlchemy
/// uses 7 field specifiers. We could probably store more inline if this turns out to be a
/// performance problem. For now, we optimize for memory usage.
const NUM_FIELD_SPECIFIERS_INLINE: usize = 1;

/// Builder to infer all types in a region.
///
/// A builder is used by creating it with [`new()`](TypeInferenceBuilder::new), and then calling
/// [`finish_expression()`](TypeInferenceBuilder::finish_expression), [`finish_definition()`](TypeInferenceBuilder::finish_definition), or [`finish_scope()`](TypeInferenceBuilder::finish_scope) on it, which returns
/// type inference result.
///
/// There are a few different kinds of methods in the type inference builder, and the naming
/// distinctions are a bit subtle.
///
/// The `finish` methods call [`infer_region`](TypeInferenceBuilder::infer_region), which delegates
/// to one of [`infer_region_scope`](TypeInferenceBuilder::infer_region_scope),
/// [`infer_region_definition`](TypeInferenceBuilder::infer_region_definition),
/// [`infer_region_function_decorators`](TypeInferenceBuilder::infer_region_function_decorators),
/// [`infer_region_deferred`](TypeInferenceBuilder::infer_region_deferred), or
/// [`infer_region_expression`](TypeInferenceBuilder::infer_region_expression), depending which
/// kind of [`InferenceRegion`] we are inferring types for.
///
/// Scope inference starts with the scope body, walking all statements and expressions and
/// recording the types of each expression in the inference result. Most of the methods
/// here (with names like `infer_*_statement` or `infer_*_expression` or some other node kind) take
/// a single AST node and are called as part of this AST visit.
///
/// When the visit encounters a node which creates a [`Definition`], we look up the definition in
/// the semantic index and call the [`infer_definition_types()`] query on it, which creates another
/// [`TypeInferenceBuilder`] just for that definition, and we merge the returned inference result
/// into the one we are currently building for the entire scope. Using the query in this way
/// ensures that if we first infer types for some scattered definitions in a scope, and later for
/// the entire scope, we don't re-infer any types, we reuse the cached inference for those
/// definitions and their sub-expressions.
///
/// Functions with a name like `infer_*_definition` take both a node and a [`Definition`], and are
/// called by [`infer_region_definition`](TypeInferenceBuilder::infer_region_definition).
///
/// So for example we have both
/// [`infer_function_definition_statement`](TypeInferenceBuilder::infer_function_definition_statement),
/// which takes just the function AST node, and
/// [`infer_function_definition`](TypeInferenceBuilder::infer_function_definition), which takes
/// both the node and the [`Definition`] id. The former is called as part of walking the AST, and
/// it just looks up the [`Definition`] for that function in the semantic index and calls
/// [`infer_definition_types()`] on it, which will create a new [`TypeInferenceBuilder`] with
/// [`InferenceRegion::Definition`], and in that builder
/// [`infer_region_definition`](TypeInferenceBuilder::infer_region_definition) will call
/// [`infer_function_definition`](TypeInferenceBuilder::infer_function_definition) to actually
/// infer a type for the definition.
///
/// Similarly, when we encounter a standalone-inferable expression (right-hand side of an
/// assignment, type narrowing guard), we use the [`infer_expression_types()`] query to ensure we
/// don't infer its types more than once.
pub(super) struct TypeInferenceBuilder<'db, 'ast> {
    context: InferContext<'db, 'ast>,

    index: &'db SemanticIndex<'db>,
    region: InferenceRegion<'db>,

    /// The types of every expression in this region.
    expressions: FxHashMap<ExpressionNodeKey, Type<'db>>,

    /// An expression cache shared across builders during multi-inference.
    expression_cache: Option<Rc<RefCell<ExpressionCache<'db>>>>,

    /// Reachability evaluations reused while inferring this region.
    ///
    /// Most inference regions never evaluate reachability, so allocate the cache lazily. Speculative
    /// builders share an initialized cache with their parent so repeated place lookups performed
    /// during multi-inference can reuse predicate truthiness computed by the parent builder.
    reachability_cache: OnceCell<Rc<ReachabilityEvaluationCache<'db>>>,

    /// Type qualifiers (`Required`, `NotRequired`, etc.) for annotation expressions.
    /// Only populated for expressions that have non-empty qualifiers.
    qualifiers: FxHashMap<ExpressionNodeKey, TypeQualifiers>,

    /// Metadata for type expressions.
    /// Only populated for expressions that have non-empty flags.
    type_expression_flags: FxHashMap<ExpressionNodeKey, TypeExpressionFlags>,

    /// The constraints on any collection initializers that are accessed in this region.
    //
    // TODO: Store projected constraint sets directly here instead of specialized receiver types.
    // Bound-method calls on unconstrained collection initializers can introduce method-local typevars
    // (for example, `list.sort` constrains `T@list` using `SupportsRichComparisonT@sort`). A
    // principled representation would store an owned constraint set over the collection initializer's
    // generic context and existentially quantify away the method-local typevars, so combining
    // `xs.append("x")` with `xs.sort()` yields `str ≤ T ≤ SupportsRichComparison` instead of
    // leaking `SupportsRichComparisonT@sort` into the inferred list element type.
    collection_use_constraints: FxHashMap<Definition<'db>, FxIndexSet<Type<'db>>>,

    /// For each use of a fluid specialization candidate that was inferred with a
    /// bidirectional type context, the contextual type.
    fluid_adoptions: FxHashMap<ExpressionNodeKey, Type<'db>>,

    /// basedpython `?.`: for each link of an optional chain, the type that link has
    /// when every `?.` receiver in the chain is present.
    ///
    /// A link's own expression type carries the `None` it short-circuits to, but the rest
    /// of the chain must resolve against the present type: `a?.b.c` lowers to
    /// `None if a is None else a.b.c`, which never reaches `.c` with an absent `a`. Folding
    /// the `None` into each link instead would leave every later `.attr`, `(...)` or `[...]`
    /// in the chain resolving against a value the lowering never produces.
    ///
    /// Only populated for links that can short-circuit, so a hit here doubles as "this
    /// expression is a chain link that contributes a `None` to the end of its chain". An
    /// attribute that is optional in its *own* right stays optional in the recorded type,
    /// so `a?.cb()` still reports a possibly-`None` `cb`.
    basedpython_chain_present: FxHashMap<ExpressionNodeKey, Type<'db>>,

    /// The creation-time type of a fluid specialization candidate, with literal types
    /// retained. Only set when this region is the standalone inference of the
    /// candidate's assigned value; uses of the binding re-solve their own prefix of the
    /// constraining events starting from this type.
    fluid_creation: Option<Type<'db>>,

    /// The resolved event timeline of a fluid specialization candidate, with cumulative
    /// solutions. Only set when this region is the standalone inference of the
    /// candidate's assigned value; uses of the binding look up their own prefix of the
    /// events here instead of re-solving it.
    fluid_timeline: Option<FluidTimeline<'db>>,

    /// Expressions that are string annotations
    string_annotations: FxHashSet<ExpressionNodeKey>,

    /// Call expressions whose type is `Never` only because the call left a type variable
    /// unsolved. Reachability analysis reads this to tell such a call apart from one that
    /// genuinely does not return.
    unsolved_typevar_calls: FxHashSet<ExpressionNodeKey>,

    /// Expected types for expression nodes tracked for IDE completion.
    expected_types: FxHashMap<ExpressionNodeKey, Type<'db>>,

    /// basedpython: the type the call a trailing lambda block stands for produces.
    /// Only set when this region is that block's decorators region, where the call
    /// is checked; a block written as a statement's value takes its type from here.
    trailing_lambda_return: Option<Type<'db>>,

    /// The scope this region is part of.
    scope: ScopeId<'db>,

    // bindings, declarations, and deferred can only exist in definition, or scope contexts.
    /// The types of every binding in this region.
    ///
    /// The list should only contain one entry per binding at most.
    bindings: VecMap<Definition<'db>, Type<'db>>,

    /// The types and type qualifiers of every valid declaration in this region.
    ///
    /// The list should only contain one entry per declaration at most.
    declarations: VecMap<Definition<'db>, TypeAndQualifiers<'db>>,

    /// The definitions with deferred sub-parts.
    ///
    /// The list should only contain one entry per definition.
    deferred: VecSet<Definition<'db>>,

    /// The returned types and their corresponding ranges of the region, if it is a function body.
    return_types_and_ranges: Vec<TypeAndRange<'db>>,

    /// A set of functions that have been defined **and** called in this region.
    ///
    /// This is a set because the same function could be called multiple times in the same region.
    /// This is mainly used in [`post_inference::overloaded_function::check_overloaded_function`] to
    /// check an overloaded function that is shadowed by a function with the same name in this
    /// scope but has been called before. For example:
    ///
    /// ```py
    /// from typing import overload
    ///
    /// @overload
    /// def foo() -> None: ...
    /// @overload
    /// def foo(x: int) -> int: ...
    /// def foo(x: int | None) -> int | None: return x
    ///
    /// foo()  # An overloaded function that was defined in this scope have been called
    ///
    /// def foo(x: int) -> int:
    ///     return x
    /// ```
    ///
    /// To keep the calculation deterministic, we use an `FxIndexSet` whose order is determined by the sequence of insertion calls.
    called_functions: FxIndexSet<FunctionType<'db>>,

    /// Whether we are in a context that binds unbound typevars.
    typevar_binding_context: Option<Definition<'db>>,

    /// The deferred state of inferring types of certain expressions within the region.
    ///
    /// This is different from [`InferenceRegion::Deferred`] which works on the entire definition
    /// while this is relevant for specific expressions within the region itself and is updated
    /// during the inference process.
    ///
    /// For example, when inferring the types of an annotated assignment, the type of an annotation
    /// expression could be deferred if the file has `from __future__ import annotations` import or
    /// is a stub file but we're still in a non-deferred region.
    deferred_state: DeferredExpressionState,

    /// For decorated function or class definitions, the type before applying decorators.
    undecorated_type: Option<Type<'db>>,

    /// The fallback type for missing expressions/bindings/declarations or recursive type inference.
    cycle_recovery: Option<Type<'db>>,

    /// If the inference region refers to a definition, whether synthesized dictionary-key
    /// assignments derived from its right-hand side should be discarded.
    discards_dict_key_assignments: bool,

    /// A list of `dataclass_transform` field specifiers that are "active" (when inferring
    /// the right hand side of an annotated assignment in a class that is a dataclass).
    dataclass_field_specifiers: SmallVec<[Type<'db>; NUM_FIELD_SPECIFIERS_INLINE]>,

    /// basedpython: set by a `ty_extensions.Top` / `Bottom` reference inside a
    /// subscript slice. the enclosing subscript reads and clears this on exit
    /// to apply the corresponding materialization to the result
    slice_materialization: Option<crate::types::MaterializationKind>,
}

fn transparent_callable_decorator_result<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    bindings: &Bindings<'db>,
    decorated_ty: Type<'db>,
) -> Option<Type<'db>> {
    enum TransparentCallableReturn<'db> {
        TypeVar(BoundTypeVarInstance<'db>),
        Awaitable(BoundTypeVarInstance<'db>),
    }

    impl<'db> TransparentCallableReturn<'db> {
        fn matches(self, db: &'db dyn Db, other: Self) -> bool {
            match (self, other) {
                (Self::TypeVar(left), Self::TypeVar(right))
                | (Self::Awaitable(left), Self::Awaitable(right)) => {
                    left.is_same_typevar_as(db, right)
                }
                _ => false,
            }
        }
    }

    fn callable_paramspec_and_return<'db>(
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        ty: Type<'db>,
    ) -> Option<(BoundTypeVarInstance<'db>, TransparentCallableReturn<'db>)> {
        let callable = ty.resolve_type_alias(db).as_callable()?;
        if callable.kind(db) != CallableTypeKind::Regular {
            return None;
        }
        let [signature] = callable.signatures(db).overloads.as_slice() else {
            return None;
        };
        let paramspec = signature.parameters().as_paramspec()?;
        let return_typevar = if let Some(typevar) = signature.return_ty.as_typevar() {
            TransparentCallableReturn::TypeVar(typevar)
        } else {
            let specialization =
                signature
                    .return_ty
                    .known_specialization(db, env, KnownClass::Awaitable)?;
            let [inner] = specialization.types(db) else {
                return None;
            };
            TransparentCallableReturn::Awaitable(inner.as_typevar()?)
        };
        Some((paramspec, return_typevar))
    }

    if !matches!(decorated_ty, Type::FunctionLiteral(_) | Type::Callable(_)) {
        return None;
    }

    let binding = bindings.single_element()?;
    let (_, overload) = binding.matching_overloads().exactly_one().ok()?;
    let decorator_signature = &overload.signature;
    let bound_signature = binding
        .bound_type
        .map(|bound_type| decorator_signature.bind_self(db, env, Some(bound_type)));
    let decorator_signature = bound_signature.as_ref().unwrap_or(decorator_signature);
    let [parameter] = decorator_signature.parameters().as_slice() else {
        return None;
    };

    let (parameter_callable_paramspec, parameter_callable_return) =
        callable_paramspec_and_return(db, env, parameter.annotated_type())?;
    let (return_callable_paramspec, return_callable_return) =
        callable_paramspec_and_return(db, env, decorator_signature.return_ty)?;
    if !parameter_callable_paramspec.is_same_typevar_as(db, return_callable_paramspec)
        || !parameter_callable_return.matches(db, return_callable_return)
    {
        return None;
    }

    match decorated_ty {
        Type::FunctionLiteral(function) => Some(Type::Callable(function.into_callable_type(db))),
        Type::Callable(_) => Some(decorated_ty),
        _ => None,
    }
}

impl<'db, 'ast> TypeInferenceBuilder<'db, 'ast> {
    /// How big a string do we build before bailing?
    ///
    /// This is a fairly arbitrary number. It should be *far* more than enough
    /// for most use cases, but we can reevaluate it later if useful.
    pub(super) const MAX_STRING_LITERAL_SIZE: usize = 4096;

    /// Creates a new builder for inferring types in a region.
    pub(super) fn new(
        db: &'db dyn Db,
        env: &'ast ProgramEnvironment<'db>,
        region: InferenceRegion<'db>,
        file: File,
        program_file: ProgramFile<'db>,
        index: &'db SemanticIndex<'db>,
        module: &'ast ParsedModuleRef,
    ) -> Self {
        let scope = region.scope(db);
        Self {
            context: InferContext::new(db, env, scope, file, program_file, module),
            index,
            region,
            scope,
            return_types_and_ranges: vec![],
            called_functions: FxIndexSet::default(),
            deferred_state: DeferredExpressionState::None,
            expressions: FxHashMap::default(),
            expression_cache: None,
            reachability_cache: OnceCell::new(),
            qualifiers: FxHashMap::default(),
            type_expression_flags: FxHashMap::default(),
            collection_use_constraints: FxHashMap::default(),
            fluid_adoptions: FxHashMap::default(),
            basedpython_chain_present: FxHashMap::default(),
            fluid_creation: None,
            fluid_timeline: None,
            string_annotations: FxHashSet::default(),
            unsolved_typevar_calls: FxHashSet::default(),
            expected_types: FxHashMap::default(),
            trailing_lambda_return: None,
            bindings: VecMap::default(),
            declarations: VecMap::default(),
            typevar_binding_context: None,
            deferred: VecSet::default(),
            undecorated_type: None,
            cycle_recovery: None,
            discards_dict_key_assignments: false,
            dataclass_field_specifiers: SmallVec::new(),
            slice_materialization: None,
        }
    }

    fn reachability_cache(&self) -> &ReachabilityEvaluationCache<'db> {
        self.reachability_cache
            .get_or_init(|| {
                let scope = self.scope();
                let reachability_constraints = self
                    .index
                    .use_def_map(scope.file_scope_id(self.db()))
                    .reachability_constraints();
                Rc::new(ReachabilityEvaluationCache::new(
                    scope,
                    reachability_constraints,
                ))
            })
            .as_ref()
    }

    fn discard_dict_key_assignments_for(&mut self, definition: Definition<'db>) {
        if matches!(self.region, InferenceRegion::Definition(d) if d == definition) {
            self.discards_dict_key_assignments = true;
        }
    }

    fn fallback_type(&self) -> Option<Type<'db>> {
        self.cycle_recovery
    }

    fn recursive_type_expression_definition(&self) -> Option<Definition<'db>> {
        self.typevar_binding_context.or(match self.region {
            InferenceRegion::Definition(definition) | InferenceRegion::Deferred(definition) => {
                Some(definition)
            }
            InferenceRegion::Statement(_)
            | InferenceRegion::Expression(_, _)
            | InferenceRegion::FunctionDecorators(_)
            | InferenceRegion::Scope(_, _) => None,
        })
    }

    fn extend_cycle_recovery(&mut self, other: Option<Type<'db>>) {
        let db = self.db();
        if let Some(other) = other {
            match self.cycle_recovery {
                Some(existing) => {
                    self.cycle_recovery = Some(UnionType::from_two_elements(
                        db,
                        self.program_environment(),
                        existing,
                        other,
                    ));
                }
                None => {
                    self.cycle_recovery = Some(other);
                }
            }
        }
    }

    fn extend_definition(
        &mut self,
        definition: Definition<'db>,
        inference: &DefinitionInference<'db>,
    ) {
        #[cfg(debug_assertions)]
        assert_eq!(self.scope, inference.scope);

        self.expressions
            .extend(inference.expressions.iter().copied());
        self.declarations.extend(inference.declarations(definition));

        if !matches!(self.region, InferenceRegion::Scope(..)) {
            self.bindings.extend(inference.bindings(definition));
        }

        if let Some(extra) = &inference.extra {
            match extra.as_ref() {
                DefinitionInferenceExtra::Qualifiers(qualifiers) => {
                    self.qualifiers.extend(qualifiers.iter().copied());
                }
                DefinitionInferenceExtra::Deferred(deferred) => {
                    self.deferred.extend(deferred.iter().copied());
                }
                DefinitionInferenceExtra::Diagnostics(diagnostics) => {
                    self.context.extend(diagnostics);
                }
                DefinitionInferenceExtra::DeferredAndUndecorated(extra) => {
                    self.deferred.extend(extra.deferred.iter().copied());
                }
                DefinitionInferenceExtra::CalledFunctions(called_functions) => {
                    self.called_functions
                        .extend(called_functions.iter().copied());
                }
                DefinitionInferenceExtra::ExpectedTypes(expected_types) => {
                    self.expected_types.extend(expected_types.iter().copied());
                }
                DefinitionInferenceExtra::StringAnnotations(string_annotations) => {
                    self.string_annotations
                        .extend(string_annotations.iter().copied());
                }
                DefinitionInferenceExtra::Undecorated(_)
                | DefinitionInferenceExtra::DiscardsDictKeyAssignments => {}
                DefinitionInferenceExtra::Other(extra) => {
                    self.called_functions
                        .extend(extra.called_functions.iter().copied());
                    self.extend_cycle_recovery(extra.cycle_recovery);
                    self.context.extend(&extra.diagnostics);
                    self.deferred.extend(extra.deferred.iter().copied());
                    self.string_annotations
                        .extend(extra.string_annotations.iter().copied());
                    self.expected_types
                        .extend(extra.expected_types.iter().copied());
                    self.qualifiers.extend(extra.qualifiers.iter().copied());
                    self.type_expression_flags
                        .extend(extra.type_expression_flags.iter().copied());

                    #[expect(
                        clippy::iter_over_hash_type,
                        reason = "constraints for distinct collection definitions are merged \
                            independently"
                    )]
                    for (collection_def, constraints) in &extra.collection_use_constraints {
                        self.collection_use_constraints
                            .entry(*collection_def)
                            .and_modify(|this| this.extend(constraints))
                            .or_insert(constraints.clone());
                    }

                    self.fluid_adoptions.extend(extra.fluid_adoptions.iter());
                }
            }
        }
    }

    fn extend_statement(&mut self, inference: &StatementInference<'db>) {
        let inference = match inference {
            StatementInference::Other(inference) => inference,
            StatementInference::Expression(inference) => return self.extend_expression(inference),
            StatementInference::Definition(definition, inference) => {
                return self.extend_definition(*definition, inference);
            }
        };

        #[cfg(debug_assertions)]
        assert_eq!(self.scope, inference.scope);

        self.expressions
            .extend(inference.expressions.iter().copied());
        self.declarations.extend(inference.declarations());

        if !matches!(self.region, InferenceRegion::Scope(..)) {
            self.bindings.extend(inference.bindings());
        }

        if let Some(extra) = &inference.extra {
            self.called_functions
                .extend(extra.called_functions.iter().copied());
            self.return_types_and_ranges
                .extend(extra.return_types_and_ranges.iter().copied());
            self.extend_cycle_recovery(extra.cycle_recovery);
            self.context.extend(&extra.diagnostics);
            self.deferred.extend(extra.deferred.iter().copied());
            self.string_annotations
                .extend(extra.string_annotations.iter().copied());
            self.expected_types
                .extend(extra.expected_types.iter().copied());
            self.qualifiers.extend(extra.qualifiers.iter().copied());
            self.type_expression_flags
                .extend(extra.type_expression_flags.iter().copied());
        }
    }

    fn extend_expression(&mut self, inference: &ExpressionInference<'db>) {
        #[cfg(debug_assertions)]
        assert_eq!(self.scope, inference.scope);

        self.extend_expression_unchecked(inference);
    }

    fn extend_expression_unchecked(&mut self, inference: &ExpressionInference<'db>) {
        self.expressions
            .extend(inference.expressions.iter().copied());

        if let Some(extra) = &inference.extra {
            self.context.extend(&extra.diagnostics);
            self.extend_cycle_recovery(extra.cycle_recovery);
            self.called_functions
                .extend(extra.called_functions.iter().copied());
            self.string_annotations
                .extend(extra.string_annotations.iter().copied());
            self.expected_types
                .extend(extra.expected_types.iter().copied());
            self.type_expression_flags
                .extend(extra.type_expression_flags.iter().copied());

            #[expect(
                clippy::iter_over_hash_type,
                reason = "constraints for distinct collection definitions are merged independently"
            )]
            for (collection_def, constraints) in &extra.collection_use_constraints {
                self.collection_use_constraints
                    .entry(*collection_def)
                    .and_modify(|this| this.extend(constraints))
                    .or_insert(constraints.clone());
            }

            self.fluid_adoptions.extend(extra.fluid_adoptions.iter());

            if !matches!(self.region, InferenceRegion::Scope(..)) {
                self.bindings.extend(extra.bindings.iter().copied());
            }
        }
    }

    fn extend_expression_cache_entry(&mut self, inference: &FullExpressionCacheEntry<'db>) {
        #[cfg(debug_assertions)]
        assert_eq!(self.scope, inference.scope);

        self.expressions
            .extend(inference.expressions.iter().map(|(key, ty)| (*key, *ty)));
        self.context.extend(&inference.diagnostics);
        self.extend_cycle_recovery(inference.cycle_recovery);
        self.called_functions
            .extend(inference.called_functions.iter().copied());
        self.string_annotations
            .extend(inference.string_annotations.iter().copied());
        self.unsolved_typevar_calls
            .extend(inference.unsolved_typevar_calls.iter().copied());
        self.expected_types
            .extend(inference.expected_types.iter().map(|(key, ty)| (*key, *ty)));
        self.type_expression_flags.extend(
            inference
                .type_expression_flags
                .iter()
                .map(|(key, flags)| (*key, *flags)),
        );

        #[expect(
            clippy::iter_over_hash_type,
            reason = "constraints for distinct collection definitions are merged independently"
        )]
        for (collection_def, constraints) in &inference.collection_use_constraints {
            self.collection_use_constraints
                .entry(*collection_def)
                .and_modify(|this| this.extend(constraints))
                .or_insert(constraints.clone());
        }

        self.fluid_adoptions
            .extend(inference.fluid_adoptions.iter());

        if !matches!(self.region, InferenceRegion::Scope(..)) {
            self.bindings.extend(
                inference
                    .bindings
                    .iter()
                    .map(|(definition, ty)| (*definition, *ty)),
            );
        }
    }

    fn extend_scope(&mut self, inference: &ScopeInference<'db>) {
        self.expressions.extend(inference.expressions.iter());

        if let Some(extra) = &inference.extra {
            self.context.extend(&extra.diagnostics);
            self.extend_cycle_recovery(extra.cycle_recovery);
            self.string_annotations
                .extend(extra.string_annotations.iter().copied());
            self.expected_types
                .extend(extra.expected_types.iter().copied());
            self.type_expression_flags
                .extend(extra.type_expression_flags.iter().copied());

            #[expect(
                clippy::iter_over_hash_type,
                reason = "constraints for distinct collection definitions are merged independently"
            )]
            for (collection_def, constraints) in &extra.collection_use_constraints {
                self.collection_use_constraints
                    .entry(*collection_def)
                    .and_modify(|this| this.extend(constraints))
                    .or_insert(constraints.clone());
            }

            self.fluid_adoptions.extend(extra.fluid_adoptions.iter());
        }
    }

    fn file(&self) -> File {
        self.context.file()
    }

    fn program_file(&self) -> ProgramFile<'db> {
        self.context.program_file()
    }

    #[inline]
    fn program_environment(&self) -> &'ast ProgramEnvironment<'db> {
        self.context.program_environment()
    }

    fn module(&self) -> &'ast ParsedModuleRef {
        self.context.module()
    }

    fn db(&self) -> &'db dyn Db {
        self.context.db()
    }

    fn scope(&self) -> ScopeId<'db> {
        self.scope
    }

    /// Returns call bindings annotated with the call site's enclosing binding contexts.
    ///
    /// Call binding uses this as an optimization hint to avoid freshening generic callable
    /// signatures when the callable's generic context cannot collide with a containing scope.
    fn bindings_for_call(&self, callable_type: Type<'db>) -> Bindings<'db> {
        let db = self.db();
        callable_type
            .bindings(db, self.program_environment())
            .with_enclosing_binding_contexts(enclosing_binding_contexts(
                self.index,
                self.scope().file_scope_id(db),
            ))
    }

    fn settings(&self) -> &AnalysisSettings {
        self.db().analysis_settings(self.file())
    }

    /// Whether the basedpython "fluid specializations" feature is active for this file.
    ///
    /// Disabled via `analysis.disable-fluid-specializations`; when disabled, inferred generic
    /// specializations are not widened flow-sensitively by later uses of a binding.
    ///
    /// Also disabled by the `TY_DISABLE_FLUID_SPECIALIZATIONS` environment variable. This exists
    /// because a config option can't be used to disable the feature in the ecosystem-analyzer
    /// workflow: that workflow feeds one config to both the base and PR binaries, and the base
    /// binary (which predates the option) hard-errors on the unknown field. An environment
    /// variable is ignored by binaries that don't know it, so it can be set for both.
    ///
    /// TODO(perf): remove this once the residual superlinear fluid re-solve cost is fixed (it
    /// still times out several large ecosystem projects); see the fluid-specialization
    /// performance investigation.
    fn fluid_specializations_enabled(&self) -> bool {
        static DISABLED_BY_ENV: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
            std::env::var_os("TY_DISABLE_FLUID_SPECIALIZATIONS").is_some()
        });
        !*DISABLED_BY_ENV && !self.settings().disable_fluid_specializations
    }

    fn is_in_type_checking_block(&self, scope: ScopeId<'db>, node: impl Ranged) -> bool {
        self.index
            .is_in_type_checking_block(scope.file_scope_id(self.db()), node.range())
    }

    /// If the current scope is a class body scope of a dataclass-like class, populate
    /// `self.dataclass_field_specifiers` with the field specifiers from the class's
    /// `dataclass_params` or `dataclass_transform` parameters. This is needed so that
    /// calls to field-specifier functions are recognized during type inference of the
    /// right-hand side of annotated assignments.
    fn setup_dataclass_field_specifiers(&mut self) {
        fn field_specifiers<'db>(
            db: &'db dyn Db,
            index: &'db SemanticIndex<'db>,
            scope: ScopeId<'db>,
        ) -> Option<SmallVec<[Type<'db>; NUM_FIELD_SPECIFIERS_INLINE]>> {
            let enclosing_scope = index.scope(scope.file_scope_id(db));
            let class_node = enclosing_scope.node().as_class()?;
            let class_definition = index.expect_single_definition(class_node);
            let class_literal = original_class_type(db, class_definition)?.as_static()?;

            class_literal
                .dataclass_params(db)
                .map(|params| SmallVec::from(params.field_specifiers(db)))
                .or_else(|| {
                    Some(SmallVec::from(
                        CodeGeneratorKind::from_class(db, class_literal.into())?
                            .field_specifiers(db)?,
                    ))
                })
        }

        if let Some(specifiers) = field_specifiers(self.db(), self.index, self.scope()) {
            self.dataclass_field_specifiers = specifiers;
        }
    }

    /// Setup a shared expression cache for multi-inference.
    ///
    /// Returns `false` if the expression cache was already initialized.
    fn setup_expression_cache(&mut self) -> bool {
        if self.expression_cache.is_some() {
            false
        } else {
            self.expression_cache = Some(Rc::new(RefCell::new(ExpressionCache::default())));
            true
        }
    }

    fn teardown_expression_cache(&mut self) {
        self.expression_cache = None;
    }

    /// Are we currently inferring types in file with deferred types?
    /// This is true for stub files, for files with `__future__.annotations`, and
    /// by default for all source files in Python 3.14 and later.
    fn defer_annotations(&self) -> bool {
        self.index.has_future_annotations()
            || self.in_stub()
            || self.is_basedpython_file()
            || self.program_environment().python_version(self.db()) >= PythonVersion::PY314
    }

    /// Are we currently in a context where name resolution should be deferred
    /// (`__future__.annotations`, stub file, or stringified annotation)?
    fn is_deferred(&self) -> bool {
        self.deferred_state.is_deferred()
    }

    /// Return the node key of the given AST node, or the key of the outermost enclosing string
    /// literal, if the node originates from inside a stringified annotation.
    fn enclosing_node_key(&self, node: AnyNodeRef<'_>) -> NodeKey {
        match self.deferred_state {
            DeferredExpressionState::InStringAnnotation(enclosing_node_key) => enclosing_node_key,
            _ => NodeKey::from_node(node),
        }
    }

    fn in_stub(&self) -> bool {
        self.context.in_stub()
    }

    fn in_string_annotation(&self) -> bool {
        self.deferred_state.in_string_annotation()
    }

    /// Returns `true` if `expr` is a call to a known diagnostic function
    /// (e.g., `reveal_type` or `assert_type`) whose return value should not
    /// trigger the `unused-awaitable` lint.
    fn is_known_function_call(&self, expr: &ast::Expr) -> bool {
        let ast::Expr::Call(call) = expr else {
            return false;
        };
        matches!(
            self.expression_type(&call.func),
            Type::FunctionLiteral(f)
                if matches!(
                    f.known(self.db()),
                    Some(KnownFunction::RevealType | KnownFunction::AssertType)
                )
        )
    }

    /// Get the already-inferred type of an expression node, or Unknown.
    fn expression_type(&self, expr: &ast::Expr) -> Type<'db> {
        self.try_expression_type(expr).unwrap_or_else(Type::unknown)
    }

    fn try_expression_type(&self, expr: &ast::Expr) -> Option<Type<'db>> {
        self.expressions
            .get(&expr.into())
            .copied()
            .or(self.fallback_type())
    }

    /// Return an already-inferred type for `expr`, or infer it with `tcx` if needed.
    ///
    /// This is used in places where an expression may already have been inferred earlier with a
    /// more specific type context, and re-inferring it would be redundant or would duplicate
    /// diagnostics.
    fn get_or_infer_expression(&mut self, expr: &ast::Expr, tcx: TypeContext<'db>) -> Type<'db> {
        self.try_expression_type(expr)
            .unwrap_or_else(|| self.infer_expression(expr, tcx))
    }

    /// Store qualifiers for an annotation expression.
    fn store_qualifiers(&mut self, expr: &ast::Expr, qualifiers: TypeQualifiers) {
        if !qualifiers.is_empty() {
            self.qualifiers.insert(expr.into(), qualifiers);
        }
    }

    /// Store metadata for a type expression.
    fn store_type_expression_flags(
        &mut self,
        expr: impl Into<ExpressionNodeKey>,
        flags: TypeExpressionFlags,
    ) {
        if flags.is_empty() {
            return;
        }

        self.type_expression_flags
            .entry(expr.into())
            .or_default()
            .insert(flags);
    }

    /// Get metadata for a type expression from the current inference result.
    fn type_expression_flags(&self, expr: impl Into<ExpressionNodeKey>) -> TypeExpressionFlags {
        self.type_expression_flags
            .get(&expr.into())
            .copied()
            .unwrap_or_default()
    }

    /// Get the type of an expression from any scope in the same file.
    ///
    /// If the expression is in the current scope, and we are inferring the entire scope, just look
    /// up the expression in our own results, otherwise call [`infer_scope_types()`] for the scope
    /// of the expression.
    ///
    /// ## Panics
    ///
    /// If the expression is in the current scope but we haven't yet inferred a type for it.
    ///
    /// Can cause query cycles if the expression is from a different scope and type inference is
    /// already in progress for that scope (further up the stack).
    fn file_expression_type(&self, expression: &ast::Expr) -> Type<'db> {
        let file_scope = self.index.expression_scope_id(expression);
        let expr_scope = file_scope.to_scope_id(self.db(), self.program_file());
        match self.region {
            InferenceRegion::Scope(scope, _) if scope == expr_scope => {
                self.expression_type(expression)
            }
            _ => infer_complete_scope_types(self.db(), expr_scope).expression_type(expression),
        }
    }

    /// Get metadata for a type expression from any scope in the same file.
    fn file_type_expression_flags(&self, expression: &ast::Expr) -> TypeExpressionFlags {
        let file_scope = self.index.expression_scope_id(expression);
        let expr_scope = file_scope.to_scope_id(self.db(), self.program_file());
        match self.region {
            InferenceRegion::Scope(scope, _) if scope == expr_scope => {
                self.type_expression_flags(expression)
            }
            _ => {
                infer_complete_scope_types(self.db(), expr_scope).type_expression_flags(expression)
            }
        }
    }

    /// Infers types in the given [`InferenceRegion`].
    fn infer_region(&mut self) {
        match self.region {
            InferenceRegion::Statement(statement) => self.infer_region_statement(statement),
            InferenceRegion::Scope(scope, tcx) => self.infer_region_scope(scope, tcx),
            InferenceRegion::Definition(definition) => self.infer_region_definition(definition),
            InferenceRegion::FunctionDecorators(definition) => {
                self.infer_region_function_decorators(definition);
            }
            InferenceRegion::Deferred(definition) => self.infer_region_deferred(definition),
            InferenceRegion::Expression(expression, tcx) => {
                self.infer_region_expression(expression, tcx);
            }
        }
    }

    fn infer_region_scope(&mut self, scope: ScopeId<'db>, tcx: TypeContext<'db>) {
        let node = scope.node(self.db());
        match node {
            NodeWithScopeKind::Module => {
                self.infer_module(self.module().syntax());
            }
            NodeWithScopeKind::Function(function) => {
                self.infer_function_body(function.node(self.module()));
            }
            NodeWithScopeKind::Lambda(lambda) => {
                self.infer_lambda_body(lambda.node(self.module()), tcx);
            }
            NodeWithScopeKind::Class(class) => self.infer_class_body(class.node(self.module())),
            NodeWithScopeKind::ClassTypeParameters(class) => {
                self.infer_class_type_params(class.node(self.module()));
            }
            NodeWithScopeKind::FunctionTypeParameters(function) => {
                self.infer_function_type_params(function.node(self.module()));
            }
            NodeWithScopeKind::TypeAliasTypeParameters(type_alias) => {
                self.infer_type_alias_type_params(type_alias.node(self.module()));
            }
            NodeWithScopeKind::TypeAlias(type_alias) => {
                self.infer_type_alias(type_alias.node(self.module()));
            }
            NodeWithScopeKind::ListComprehension(comprehension) => {
                self.infer_list_comprehension_expression_scope(
                    comprehension.node(self.module()),
                    tcx,
                );
            }
            NodeWithScopeKind::SetComprehension(comprehension) => {
                self.infer_set_comprehension_expression_scope(
                    comprehension.node(self.module()),
                    tcx,
                );
            }
            NodeWithScopeKind::DictComprehension(comprehension) => {
                self.infer_dict_comprehension_expression_scope(
                    comprehension.node(self.module()),
                    tcx,
                );
            }
            NodeWithScopeKind::GeneratorExpression(generator) => {
                self.infer_generator_expression_scope(generator.node(self.module()), tcx);
            }
        }

        // Infer deferred types for all definitions.
        let deferred_definitions: Vec<_> = std::mem::take(&mut self.deferred).into_iter().collect();
        for definition in &deferred_definitions {
            self.extend_definition(*definition, infer_deferred_types(self.db(), *definition));
        }

        assert!(
            self.deferred.is_empty(),
            "Inferring deferred types should not add more deferred definitions"
        );

        if self.db().should_check_file(self.file()) {
            let mut seen_overloaded_places = FxHashSet::default();
            let mut seen_public_functions = FxHashSet::default();

            for (&definition, ty_and_quals) in &self.declarations {
                let ty = ty_and_quals.inner_type();
                match definition.kind(self.db()) {
                    DefinitionKind::Function(function) => {
                        post_inference::function::check_function_definition(
                            &self.context,
                            definition,
                            &|expr| self.file_expression_type(expr),
                        );
                        post_inference::overloaded_function::check_overloaded_function(
                            &self.context,
                            ty,
                            definition,
                            self.scope.scope(self.db()).node(),
                            self.index,
                            &mut seen_overloaded_places,
                            &mut seen_public_functions,
                        );
                        post_inference::typeguard::check_type_guard_definition(
                            &self.context,
                            ty,
                            function.node(self.module()),
                        );
                    }
                    DefinitionKind::Class(class_node) => {
                        let original_ty = match self.region {
                            InferenceRegion::Definition(current) if current == definition => {
                                self.undecorated_type
                            }
                            _ => original_class_type(self.db(), definition).map(Type::ClassLiteral),
                        };
                        let ty = original_ty.unwrap_or(ty);
                        post_inference::static_class::check_static_class_definitions(
                            &self.context,
                            ty,
                            class_node.node(self.module()),
                            self.index,
                            &|expr| self.file_expression_type(expr),
                        );
                    }
                    DefinitionKind::TypeAlias(type_alias) => {
                        // an alias's variance comes from what it expands to, which is not
                        // known while the alias is being defined — so a declared variance
                        // is checked against it here, as a class's is
                        if let Type::KnownInstance(KnownInstanceType::TypeAliasType(alias)) = ty
                            && let Some(type_params) =
                                type_alias.node(self.module()).type_params.as_deref()
                        {
                            post_inference::type_param_validation::check_declared_alias_variance(
                                &self.context,
                                alias,
                                type_params,
                            );
                        }
                    }
                    DefinitionKind::AnnotatedAssignment(assignment) => {
                        if let Some(diagnostics) =
                            post_inference::pep_613_alias::check_pep_613_alias(
                                assignment, definition, self,
                            )
                        {
                            self.context.extend(&diagnostics);
                        }
                    }
                    _ => {}
                }
            }

            for definition in &deferred_definitions {
                post_inference::dynamic_class::check_dynamic_class_definition(
                    &self.context,
                    *definition,
                );
            }

            for function in &self.called_functions {
                post_inference::overloaded_function::check_overloaded_function(
                    &self.context,
                    Type::FunctionLiteral(*function),
                    function.definition(self.db()),
                    self.scope.scope(self.db()).node(),
                    self.index,
                    &mut seen_overloaded_places,
                    &mut seen_public_functions,
                );
            }

            post_inference::final_variable::check_final_without_value(&self.context, self.index);
        }
    }

    fn infer_region_statement(&mut self, statement: StatementInner<'db>) {
        self.infer_statement(statement.node_ref(self.db()).node(self.module()));
    }

    fn infer_region_definition(&mut self, definition: Definition<'db>) {
        match definition.kind(self.db()) {
            DefinitionKind::Function(function) => {
                self.infer_function_definition(function.node(self.module()), definition);
            }
            DefinitionKind::Class(class) => {
                self.infer_class_definition(class.node(self.module()), definition);
            }
            DefinitionKind::TypeAlias(type_alias) => {
                self.infer_type_alias_definition(type_alias.node(self.module()), definition);
            }
            DefinitionKind::Import(import) => {
                self.infer_import_definition(import.alias(self.module()), definition);
            }
            DefinitionKind::ImportFrom(import_from) => {
                self.infer_import_from_definition(
                    import_from.import(self.module()),
                    import_from.alias(self.module()),
                    definition,
                );
            }
            DefinitionKind::ImportFromSubmodule(import_from) => {
                self.infer_import_from_submodule_definition(
                    import_from.import(self.module()),
                    definition,
                );
            }
            DefinitionKind::StarImport(import) => {
                self.infer_import_from_definition(
                    import.import(self.module()),
                    import.alias(self.module()),
                    definition,
                );
            }
            DefinitionKind::Assignment(assignment) => {
                self.infer_assignment_definition(assignment, definition);
            }
            DefinitionKind::AnnotatedAssignment(annotated_assignment) => {
                self.infer_annotated_assignment_definition(annotated_assignment, definition);
            }
            DefinitionKind::AugmentedAssignment(augmented_assignment) => {
                self.infer_augment_assignment_definition(
                    augmented_assignment.node(self.module()),
                    definition,
                );
            }
            DefinitionKind::DictKeyAssignment(dict_key_assignment) => {
                self.infer_dict_key_assignment_definition(
                    dict_key_assignment.key(self.module()),
                    dict_key_assignment.value(self.module()),
                    dict_key_assignment.assignment(),
                    definition,
                );
            }
            DefinitionKind::For(for_statement_definition) => {
                self.infer_for_statement_definition(for_statement_definition, definition);
            }
            DefinitionKind::NamedExpression(named_expression) => {
                self.infer_named_expression_definition(
                    named_expression.node(self.module()),
                    definition,
                );
            }
            DefinitionKind::StatementExpressionValue(value) => {
                // a branch's value is an arbitrary expression, so it may be one
                // the index registered as standalone — a call, a comprehension.
                // inferring those through `infer_expression` double-infers
                let ty = self.infer_maybe_standalone_expression(
                    value.node(self.module()),
                    TypeContext::default(),
                );
                self.bindings.insert(definition, ty);
            }
            DefinitionKind::Comprehension(comprehension) => {
                self.infer_comprehension_definition(comprehension, definition);
            }
            DefinitionKind::Parameter(
                ParameterDefinitionNodeKind::VariadicPositionalParameter(parameter),
            ) => {
                self.infer_variadic_positional_parameter_definition(
                    parameter.node(self.module()),
                    definition,
                );
            }
            DefinitionKind::Parameter(ParameterDefinitionNodeKind::VariadicKeywordParameter(
                parameter,
            )) => {
                self.infer_variadic_keyword_parameter_definition(
                    parameter.node(self.module()),
                    definition,
                );
            }
            DefinitionKind::Parameter(ParameterDefinitionNodeKind::Parameter(
                parameter_with_default,
            )) => {
                self.infer_parameter_definition(
                    parameter_with_default.node(self.module()),
                    definition,
                );
            }
            DefinitionKind::LambdaParameter(LambdaParameterDefinitionNodeKind {
                index,
                lambda,
                parameter: ParameterDefinitionNodeKind::VariadicPositionalParameter(parameter),
            }) => {
                self.infer_variadic_positional_lambda_parameter_definition(
                    *index,
                    parameter.node(self.module()),
                    lambda.node(self.module()),
                    definition,
                );
            }
            DefinitionKind::LambdaParameter(LambdaParameterDefinitionNodeKind {
                parameter: ParameterDefinitionNodeKind::VariadicKeywordParameter(parameter),
                ..
            }) => {
                self.infer_variadic_keyword_lambda_parameter_definition(
                    parameter.node(self.module()),
                    definition,
                );
            }
            DefinitionKind::LambdaParameter(LambdaParameterDefinitionNodeKind {
                index,
                lambda,
                parameter: ParameterDefinitionNodeKind::Parameter(parameter_with_default),
            }) => {
                self.infer_lambda_parameter_definition(
                    *index,
                    parameter_with_default.node(self.module()),
                    lambda.node(self.module()),
                    definition,
                );
            }
            DefinitionKind::WithItem(with_item_definition) => {
                self.infer_with_item_definition(with_item_definition, definition);
            }
            DefinitionKind::MatchPattern(match_pattern) => {
                self.infer_match_pattern_definition(
                    match_pattern.pattern(self.module()),
                    match_pattern.predicate(),
                    definition,
                );
            }
            DefinitionKind::ExceptHandler(except_handler_definition) => {
                self.infer_except_handler_definition(except_handler_definition, definition);
            }
            DefinitionKind::TypeVar(node) => {
                self.infer_typevar_definition(node.node(self.module()), definition);
            }
            DefinitionKind::ParamSpec(node) => {
                self.infer_paramspec_definition(node.node(self.module()), definition);
            }
            DefinitionKind::TypeVarTuple(node) => {
                self.infer_typevartuple_definition(node.node(self.module()), definition);
            }
            DefinitionKind::TypeMatchCapture(capture) => {
                self.infer_type_match_capture_definition(
                    capture.identifier(self.module()),
                    capture.is_variadic(),
                    definition,
                );
            }
            DefinitionKind::LoopHeader(loop_header) => {
                self.infer_loop_header_definition(loop_header, definition);
            }
            DefinitionKind::NestedBindings(nested_bindings) => {
                self.infer_nested_bindings_definition(nested_bindings, definition);
            }
        }
    }

    fn infer_region_function_decorators(&mut self, definition: Definition<'db>) {
        let DefinitionKind::Function(function) = definition.kind(self.db()) else {
            return;
        };
        let function_node = function.node(self.module());

        // basedpython: a trailing lambda block's single synthetic decorator
        // holds the called expression, not a decorator — check the call (with
        // the lambda appended) instead of inferring a decoration
        if function_node.is_trailing_lambda {
            self.infer_trailing_lambda_marker(function_node);
            return;
        }

        for decorator in &function_node.decorator_list {
            let decorator_type = self.infer_decorator(decorator);
            if let Type::FunctionLiteral(function) = decorator_type
                && let Some(KnownFunction::NoTypeCheck) = function.known(self.db())
            {
                // Match `infer_function_definition`: suppress diagnostics that follow
                // `@no_type_check`, including later decorators.
                self.context.inference_flags |= InferenceFlags::IN_NO_TYPE_CHECK;
            }
        }
    }

    /// basedpython: check a trailing lambda statement's call — the expression
    /// carried by its synthetic decorator — with the lambda appended as its
    /// last argument. The argument is bound by keyword (the callee's last
    /// declared parameter) when the signature is inspectable, mirroring the
    /// lowering; positionally otherwise.
    ///
    /// Its type is the gradual callable `(...) -> Unknown` — enough to reject a
    /// parameter that is not callable at all (a block bound to `extra:
    /// dict[str, str]` has nowhere to go, and its body would vanish from the
    /// program), and gradual everywhere else: the lambda's `it` parameter is
    /// context-typed separately, and the block's return is checked by
    /// [`TRAILING_LAMBDA_RETURN_TYPE`](crate::types::diagnostic::TRAILING_LAMBDA_RETURN_TYPE),
    /// which a concrete return here would duplicate.
    ///
    /// Diagnostics anchor on the decorator node — never the call expression —
    /// so argument-index lookups can't reach for the synthetic argument,
    /// which has no AST node.
    fn infer_trailing_lambda_marker(&mut self, function: &ast::StmtFunctionDef) {
        let env = self.program_environment();
        let Some(signature_callee) = function.trailing_lambda_callee() else {
            return;
        };
        let Some(decorator) = function.decorator_list.first() else {
            return;
        };
        let expression = &decorator.expression;
        // `await f(x):` hangs the block on the call — awaiting is what the caller
        // does with what the call returns
        let awaited = match expression {
            ast::Expr::Await(await_expr) => Some(await_expr.value.as_ref()),
            _ => None,
        };
        let called = awaited.unwrap_or(expression);

        // the callee is a standalone expression (see the semantic index
        // builder), so the lambda's `it` parameter inference shares this
        // result without a cycle through the decorators region
        let callee_ty = self.infer_standalone_expression(signature_callee, TypeContext::default());

        let mut items: Vec<(Argument<'_>, Option<Type<'db>>)> = Vec::new();
        let marker_call = match called {
            ast::Expr::Call(call) if std::ptr::eq(signature_callee, call.func.as_ref()) => {
                Some(call)
            }
            _ => None,
        };
        if let Some(call) = marker_call {
            for arg_or_keyword in call.arguments.iter_source_order() {
                let item = match arg_or_keyword {
                    ast::ArgOrKeyword::Arg(argument) => match argument {
                        ast::Expr::Starred(ast::ExprStarred { value, .. }) => {
                            let ty = self.infer_expression(value, TypeContext::default());
                            self.store_expression_type(argument, ty);
                            (Argument::Variadic, Some(ty))
                        }
                        _ => (
                            Argument::Positional,
                            Some(self.infer_expression(argument, TypeContext::default())),
                        ),
                    },
                    ast::ArgOrKeyword::Keyword(ast::Keyword { arg, value, .. }) => {
                        let ty = self.infer_expression(value, TypeContext::default());
                        match arg {
                            Some(name) => (Argument::Keyword(&name.id), Some(ty)),
                            None => (Argument::Keywords, Some(ty)),
                        }
                    }
                };
                items.push(item);
            }
        }

        let keyword = trailing_lambda_keyword(self.db(), callee_ty);
        let block_ty = Type::single_callable(
            self.db(),
            Signature::new(Parameters::gradual_form(), Type::unknown()),
        );
        items.push((
            match &keyword {
                Some(name) => Argument::Keyword(name),
                None => Argument::Positional,
            },
            Some(block_ty),
        ));
        let call_arguments: CallArguments<'_, 'db> = items.into_iter().collect();

        let return_ty = match callee_ty.try_call(self.db(), env, &call_arguments) {
            Ok(bindings) => bindings.return_type(self.db(), env),
            Err(error) => {
                error.1.report_diagnostics(&self.context, decorator.into());
                error.return_type(self.db(), env)
            }
        };
        if marker_call.is_some() {
            self.store_expression_type(called, return_ty);
        }
        // the `await` reads through the call's result, and it is the awaited type
        // that the statement — and anything reading the block's value — sees
        let return_ty = if awaited.is_some() {
            let awaited_ty = return_ty.try_await(self.db(), env).unwrap_or_else(|err| {
                err.report_diagnostic(&self.context, return_ty, called.into());
                Type::unknown()
            });
            self.store_expression_type(expression, awaited_ty);
            awaited_ty
        } else {
            return_ty
        };
        // a block written as a statement's value takes this as its type; the bare
        // callee form has no call node of its own to read it back from
        self.trailing_lambda_return = Some(return_ty);
    }

    fn infer_region_deferred(&mut self, definition: Definition<'db>) {
        // N.B. We don't defer the types for an annotated assignment here because it is done in
        // the same definition query. It utilizes the deferred expression state instead.
        //
        // This is because for partially stringified annotations like `a: tuple[int, "ForwardRef"]`,
        // we need to defer the types of non-stringified expressions like `tuple` and `int` in the
        // definition query while the stringified expression `"ForwardRef"` would need to deferred
        // to use end-of-scope semantics. This would require custom and possibly a complex
        // implementation to allow this "split" to happen.

        match definition.kind(self.db()) {
            DefinitionKind::Function(function) => {
                self.infer_function_deferred(definition, function.node(self.module()));
            }
            DefinitionKind::Class(class) => {
                self.infer_class_deferred(definition, class.node(self.module()));
            }
            DefinitionKind::TypeVar(typevar) => {
                self.infer_typevar_deferred(typevar.node(self.module()));
            }
            DefinitionKind::ParamSpec(paramspec) => {
                self.infer_paramspec_deferred(paramspec.node(self.module()));
            }
            DefinitionKind::TypeVarTuple(typevartuple) => {
                self.infer_typevartuple_deferred(typevartuple.node(self.module()));
            }
            DefinitionKind::Assignment(assignment) => {
                self.infer_assignment_deferred(
                    assignment.target(self.module()),
                    assignment.value(self.module()),
                );
            }
            _ => {}
        }
    }

    fn infer_region_expression(&mut self, expression: Expression<'db>, tcx: TypeContext<'db>) {
        self.setup_dataclass_field_specifiers();

        match expression.kind(self.db()) {
            ExpressionKind::Normal => {
                self.infer_expression_impl(expression.node_ref(self.db()).node(self.module()), tcx);
            }
            ExpressionKind::TypeExpression => {
                self.infer_type_expression(expression.node_ref(self.db()).node(self.module()));
            }
        }
    }

    /// Add a binding for the given definition.
    ///
    /// Returns the result of the `infer_value_ty` closure, which is called with the declared type
    /// as type context.
    fn add_binding<'a>(
        &mut self,
        node: AnyNodeRef<'a>,
        binding: Definition<'db>,
    ) -> AddBinding<'db, 'a> {
        let db = self.db();
        debug_assert!(
            binding
                .kind(db)
                .category(self.context.in_stub(), self.module())
                .is_binding()
        );

        let db = self.db();
        let file_scope_id = binding.file_scope(db);
        let place_table = self.index.place_table(file_scope_id);
        let use_def = self.index.use_def_map(file_scope_id);

        let place_id = binding.place(self.db());
        let place = place_table.place(place_id);

        let (declarations, is_local) = if let Some(symbol) = place_id.as_symbol()
            && let Some((owner_scope, owner_symbol)) =
                self.forwarded_assignment_owner(file_scope_id, symbol)
        {
            (
                self.index
                    .use_def_map(owner_scope)
                    .end_of_scope_symbol_declarations(owner_symbol),
                false,
            )
        } else {
            (use_def.declarations_at_binding(binding), true)
        };

        let env = self.program_environment();
        let (mut place_and_quals, conflicting) = place_from_declarations_with_reachability_cache(
            db,
            env,
            declarations,
            self.reachability_cache(),
        )
        .into_place_and_conflicting_declarations();

        if let Some(conflicting) = conflicting {
            // TODO point out the conflicting declarations in the diagnostic?
            let place = place_table.place(binding.place(db));
            if let Some(builder) = self.context.report_lint(&CONFLICTING_DECLARATIONS, node) {
                builder.into_diagnostic(format_args!(
                    "Conflicting declared types for `{place}`: {}",
                    format_enumeration(conflicting.iter().map(|ty| ty.display(db, env)))
                ));
            }
        }

        // Fall back to implicit module globals for (possibly) unbound names
        if !place_and_quals.place.is_definitely_bound()
            && let PlaceExprRef::Symbol(symbol) = place
        {
            let symbol_id = place_id.expect_symbol();

            if self.skip_non_global_scopes(file_scope_id, symbol_id)
                || self.scope.file_scope_id(self.db()).is_global()
            {
                place_and_quals = place_and_quals.or_fall_back_to(db, env, || {
                    module_type_implicit_global_declaration(db, env, symbol.name())
                });
            }
        }

        let PlaceAndQualifiers {
            place: resolved_place,
            qualifiers,
        } = place_and_quals;

        let declared_ty = if resolved_place.is_undefined() && !place.is_symbol() {
            self.fallback_member_declared_type(node)
        } else {
            None
        }
        .or_else(|| resolved_place.ignore_possibly_undefined());

        AddBinding {
            declared_ty,
            binding,
            node,
            qualifiers,
            is_local,
        }
    }

    /// For a member binding without a live place declaration, obtain its declared type from
    /// normal attribute or subscript lookup on its receiver.
    fn fallback_member_declared_type(&mut self, node: AnyNodeRef<'_>) -> Option<Type<'db>> {
        let db = self.db();
        if let AnyNodeRef::ExprAttribute(ast::ExprAttribute { value, attr, .. }) = node {
            let value_type = self.try_expression_type(value).unwrap_or_else(|| {
                self.infer_maybe_standalone_expression(value, TypeContext::default())
            });
            if let Place::Defined(DefinedPlace {
                ty,
                definedness: Definedness::AlwaysDefined,
                ..
            }) = value_type
                .member(db, self.program_environment(), attr)
                .place
            {
                // TODO: also consider qualifiers on the attribute
                Some(ty)
            } else {
                None
            }
        } else if let AnyNodeRef::ExprSubscript(
            subscript @ ast::ExprSubscript {
                value, slice, ctx, ..
            },
        ) = node
        {
            let value_ty = self.get_or_infer_expression(value, TypeContext::default());
            let slice_ty = self.get_or_infer_expression(slice, TypeContext::default());
            Some(
                self.infer_subscript_expression_types(
                    subscript,
                    value_ty,
                    slice_ty,
                    *ctx,
                    TypeContext::default(),
                )
                .unwrap_or_else(|recovery_ty| recovery_ty),
            )
        } else {
            None
        }
    }

    /// Returns the owner of an assignment redirected by `global` or `nonlocal`.
    ///
    /// `global` assignments target the module symbol, while `nonlocal` assignments target the
    /// closest owning function-like scope. Local assignments and forwarding declarations whose
    /// owner cannot be resolved return `None`.
    ///
    /// ```python
    /// x = 0
    ///
    /// def outer():
    ///     y = 0
    ///
    ///     def inner():
    ///         global x
    ///         nonlocal y
    ///         x = 1  # owned by the module scope
    ///         y = 1  # owned by `outer`
    /// ```
    fn forwarded_assignment_owner(
        &self,
        scope: FileScopeId,
        symbol: ScopedSymbolId,
    ) -> Option<(FileScopeId, ScopedSymbolId)> {
        let scoped_symbol = self.index.place_table(scope).symbol(symbol);

        if scope.is_global() || scoped_symbol.is_local() {
            return None;
        }

        if scoped_symbol.is_global() {
            let global_scope = FileScopeId::global();
            // If this variable appears in a `global` declaration but has no explicit binding in
            // the global scope, return `None` so the caller can fall back to the local scope.
            return self
                .index
                .place_table(global_scope)
                .symbol_id(scoped_symbol.name())
                .map(|symbol| (global_scope, symbol));
        }

        debug_assert!(scoped_symbol.is_nonlocal());

        // Walk up parent scopes looking for the enclosing scope that defines this name.
        // `ancestor_scopes` includes the current scope, so skip that one.
        for (enclosing_scope, enclosing) in self.index.ancestor_scopes(scope).skip(1) {
            // Ignore class scopes and the global scope.
            if !enclosing.kind().is_function_like() {
                continue;
            }
            let place_table = self.index.place_table(enclosing_scope);
            let Some(enclosing_symbol) = place_table.symbol_id(scoped_symbol.name()) else {
                // This ancestor scope doesn't have a binding. Keep going.
                continue;
            };
            let symbol = place_table.symbol(enclosing_symbol);
            if symbol.is_global() {
                // The variable is `global` in this ancestor scope. This breaks the `nonlocal`
                // chain, and it's a syntax error in `infer_nonlocal_statement`. Ignore that here
                // and bail out of this loop.
                break;
            }
            if !symbol.is_local() {
                // The variable is either explicitly `nonlocal` or just a free read in this
                // ancestor scope. Keep going.
                continue;
            }

            // We found the closest definition. Note that (as in `infer_place_load`) this does not
            // need to be a binding. It could be just a declaration, e.g. `x: int`.
            return Some((enclosing_scope, enclosing_symbol));
        }

        // If no ancestor owns the name, return `None` so the caller can fall back to the local
        // scope. This will also be reported as a syntax error in `infer_nonlocal_statement`.
        None
    }

    /// Returns `true` if `symbol_id` should be looked up in the global scope, skipping intervening
    /// local scopes.
    fn skip_non_global_scopes(
        &self,
        file_scope_id: FileScopeId,
        symbol_id: ScopedSymbolId,
    ) -> bool {
        !file_scope_id.is_global()
            && self
                .index
                .symbol_is_global_in_scope(symbol_id, file_scope_id)
    }

    fn add_declaration(
        &mut self,
        node: AnyNodeRef,
        declaration: Definition<'db>,
        ty: TypeAndQualifiers<'db>,
    ) {
        let db = self.db();
        debug_assert!(
            declaration
                .kind(self.db())
                .category(self.context.in_stub(), self.module())
                .is_declaration()
        );
        let use_def = self.index.use_def_map(declaration.file_scope(self.db()));
        let prior_bindings = use_def.bindings_at_definition(declaration);
        let env = self.program_environment();
        // unbound_ty is Never because for this check we don't care about unbound
        let inferred_ty = place_from_bindings_with_reachability_cache(
            db,
            env,
            prior_bindings,
            self.reachability_cache(),
        )
        .place
        .with_qualifiers(TypeQualifiers::empty())
        .or_fall_back_to(db, env, || {
            // Fallback to bindings declared on `types.ModuleType` if it's a global symbol
            let scope = self.scope().file_scope_id(self.db());
            let place = self
                .index
                .place_table(scope)
                .place(declaration.place(self.db()));

            if let PlaceExprRef::Symbol(symbol) = &place
                && scope.is_global()
            {
                module_type_implicit_global_symbol(db, self.program_file(), symbol.name())
            } else {
                Place::Undefined.into()
            }
        })
        .place
        .ignore_possibly_undefined()
        .unwrap_or(Type::Never);
        let ty = if inferred_ty.is_assignable_to(db, env, ty.inner_type()) {
            ty
        } else {
            if let Some(builder) = self.context.report_lint(&INVALID_DECLARATION, node) {
                builder.into_diagnostic(format_args!(
                    "Cannot declare type `{}` for inferred type `{}`",
                    ty.inner_type().display(db, env),
                    inferred_ty.display(db, env)
                ));
            }
            TypeAndQualifiers::declared(Type::unknown())
        };
        self.declarations.insert(declaration, ty);
    }

    fn add_declaration_with_binding(
        &mut self,
        node: AnyNodeRef,
        definition: Definition<'db>,
        declared_and_inferred_ty: &DeclaredAndInferredType<'db>,
    ) {
        let db = self.db();
        debug_assert!(
            definition
                .kind(self.db())
                .category(self.context.in_stub(), self.module())
                .is_binding()
        );
        debug_assert!(
            definition
                .kind(self.db())
                .category(self.context.in_stub(), self.module())
                .is_declaration()
        );

        let (declared_ty, inferred_ty) = match *declared_and_inferred_ty {
            DeclaredAndInferredType::AreTheSame(type_and_qualifiers) => {
                (type_and_qualifiers, type_and_qualifiers.inner_type())
            }
            DeclaredAndInferredType::MightBeDifferent {
                declared_ty,
                inferred_ty,
            } => {
                let env = self.program_environment();
                let file_scope_id = self.scope().file_scope_id(self.db());
                if file_scope_id.is_global() {
                    let place_table = self.index.place_table(file_scope_id);
                    let place = place_table.place(definition.place(self.db()));
                    if let Some(module_type_implicit_declaration) = place
                        .as_symbol()
                        .map(|symbol| {
                            module_type_implicit_global_symbol(
                                db,
                                self.program_file(),
                                symbol.name(),
                            )
                        })
                        .and_then(|place| place.place.ignore_possibly_undefined())
                    {
                        let declared_type = declared_ty.inner_type();
                        if !declared_type.is_assignable_to(
                            db,
                            env,
                            module_type_implicit_declaration,
                        ) {
                            if let Some(builder) =
                                self.context.report_lint(&INVALID_DECLARATION, node)
                            {
                                let mut diagnostic = builder.into_diagnostic(format_args!(
                                    "Cannot shadow implicit global attribute `{place}` \
                                    with declaration of type `{}`",
                                    declared_type.display(db, env)
                                ));
                                diagnostic.info(format_args!(
                                    "The global symbol `{}` \
                                    must always have a type assignable to `{}`",
                                    place,
                                    module_type_implicit_declaration.display(db, env)
                                ));
                            }
                        }
                    }
                }
                let declared_type = declared_ty.inner_type();
                if inferred_ty.is_assignable_to(db, env, declared_type) {
                    report_bool_as_int_assignment(
                        &self.context,
                        node,
                        definition,
                        declared_type,
                        inferred_ty,
                    );
                    // TODO We currently can't distinguish here between "no declared type" and
                    // "declared types is `Unknown` (e.g. due to a bad annotation, missing
                    // import, etc.)". Ideally we would still prefer `Unknown` declared type,
                    // but use inferred type if there is no declared type.
                    if !should_preserve_inferred_binding_type(self.db(), inferred_ty)
                        && !matches!(declared_type, Type::Dynamic(DynamicType::Unknown))
                        && declared_type.is_assignable_to(db, env, inferred_ty)
                    {
                        (declared_ty, declared_type)
                    } else {
                        (declared_ty, inferred_ty)
                    }
                } else {
                    self.discard_dict_key_assignments_for(definition);
                    report_invalid_assignment(
                        &self.context,
                        node,
                        definition,
                        declared_type,
                        inferred_ty,
                    );

                    // if the assignment is invalid, fall back to assuming the annotation is correct
                    (declared_ty, declared_type)
                }
            }
        };

        self.declarations.insert(definition, declared_ty);
        self.bindings.insert(definition, inferred_ty);
    }

    fn add_unknown_declaration_with_binding(
        &mut self,
        node: AnyNodeRef,
        definition: Definition<'db>,
    ) {
        self.add_declaration_with_binding(
            node,
            definition,
            &DeclaredAndInferredType::are_the_same_type(Type::unknown()),
        );
    }

    fn record_return_type(&mut self, ty: Type<'db>, range: TextRange) {
        self.return_types_and_ranges
            .push(TypeAndRange { ty, range });
    }

    fn infer_module(&mut self, module: &ast::ModModule) {
        self.infer_body(&module.body);

        // basedpython: a trailing-lambda block in a module-level loop that
        // captures a loop variable is a late-binding trap unless its callee
        // confines it (`local` / `once`) — the type-aware complement to `B023`
        crate::types::lifetimes::check_loop_variable_capture(&self.context, &module.body, |expr| {
            self.try_expression_type(expr)
        });
    }

    fn infer_type_alias_type_params(&mut self, type_alias: &ast::StmtTypeAlias) {
        let type_params = type_alias
            .type_params
            .as_ref()
            .expect("type alias type params scope without type params");

        let binding_context = self.index.expect_single_definition(type_alias);
        let previous_typevar_binding_context =
            self.typevar_binding_context.replace(binding_context);
        self.infer_type_parameters(type_params, TypeParamReification::TypeAlias);
        self.typevar_binding_context = previous_typevar_binding_context;
    }

    fn infer_type_alias(&mut self, type_alias: &ast::StmtTypeAlias) {
        let db = self.db();
        let previous_check_unbound_typevars = self
            .context
            .inference_flags
            .replace(InferenceFlags::CHECK_UNBOUND_TYPEVARS, true);
        self.context.inference_flags |= InferenceFlags::IN_TYPE_ALIAS;
        // basedpython: a match type's `value` is the subject its `case` patterns are matched
        // against, and an unpacked pack (`match *Shape:`) is the ordinary way to write one.
        // an ordinary alias's value is left alone — its flags are whatever the surrounding
        // inference set, and forcing one either way here would change unrelated behaviour
        let value_ty = if type_alias.cases.is_empty() {
            self.infer_type_expression(&type_alias.value)
        } else {
            let previously_in_valid_unpack_context = self
                .context
                .inference_flags
                .replace(InferenceFlags::IN_VALID_UNPACK_CONTEXT, true);
            let value_ty = self.infer_type_expression(&type_alias.value);
            self.context.inference_flags.set(
                InferenceFlags::IN_VALID_UNPACK_CONTEXT,
                previously_in_valid_unpack_context,
            );
            value_ty
        };
        self.infer_match_type_cases(type_alias);
        self.context
            .inference_flags
            .remove(InferenceFlags::IN_TYPE_ALIAS);
        self.context.inference_flags.set(
            InferenceFlags::CHECK_UNBOUND_TYPEVARS,
            previous_check_unbound_typevars,
        );

        // A type alias where a value type points to itself, i.e. the expanded type is `Divergent` is meaningless
        // (but a type alias that expands to something like `list[Divergent]` may be a valid recursive type alias)
        // and would lead to infinite recursion. Therefore, such type aliases should not be exposed.
        // ```python
        // type Itself = Itself  # error: "Cyclic definition of `Itself`"
        // type A = B  # error: "Cyclic definition of `A`"
        // type B = A  # error: "Cyclic definition of `B`"
        // type G[T] = G[T]  # error: "Cyclic definition of `G`"
        // type RecursiveList[T] = list[T | RecursiveList[T]]  # OK
        // type RecursiveList2[T] = list[RecursiveList2[T]]  # It's not possible to create an element of this, but it's not an error for now
        // type IntOr = int | IntOr  # It's redundant, but OK for now
        // type IntOrStr = int | StrOrInt  # It's redundant, but OK
        // type StrOrInt = str | IntOrStr  # It's redundant, but OK
        // ```
        let expanded = value_ty.expand_eagerly(db, self.program_environment());
        if expanded.is_divergent() {
            if let Some(builder) = self
                .context
                .report_lint(&CYCLIC_TYPE_ALIAS_DEFINITION, type_alias)
            {
                builder.into_diagnostic(format_args!(
                    "Cyclic definition of `{}`",
                    type_alias.name.as_name_expr().unwrap().id,
                ));
            }
            // Replace with `Divergent`.
            self.expressions
                .insert(type_alias.value.as_ref().into(), expanded);
        }
    }

    /// basedpython: infers the `case` blocks of a match type alias.
    ///
    /// Each body is inferred once, as a type expression written in terms of that case's
    /// captures. Applying the alias only substitutes those captures, so this is where the
    /// bodies are checked and where an unusable pattern is reported.
    fn infer_match_type_cases(&mut self, type_alias: &ast::StmtTypeAlias) {
        for case in &type_alias.cases {
            self.report_invalid_match_type_pattern(&case.pattern);
            self.report_inconsistent_match_type_captures(&case.pattern);
            for stmt in &case.body {
                // any other body is a parse error, already reported
                if let ast::Stmt::Expr(expression) = stmt {
                    self.infer_type_expression(&expression.value);
                }
            }
        }
    }

    /// basedpython: reports the pattern forms a match type cannot decide.
    ///
    /// A type-level match takes apart a sequence of types, so sequence, capture, literal and
    /// or-patterns all mean something. A class or mapping pattern destructures a *value*, and
    /// a bare `*Rest` has no sequence to spread — neither has a meaning here.
    fn report_invalid_match_type_pattern(&mut self, pattern: &ast::Pattern) {
        match pattern {
            ast::Pattern::MatchClass(_) | ast::Pattern::MatchMapping(_) => {
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, pattern) {
                    builder.into_diagnostic(
                        "A match type's `case` cannot use a class or mapping pattern; \
                         it matches types, not values",
                    );
                }
            }
            ast::Pattern::MatchStar(_) => {
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, pattern) {
                    builder.into_diagnostic(
                        "A starred pattern is only valid inside a sequence pattern",
                    );
                }
            }
            ast::Pattern::MatchSequence(ast::PatternMatchSequence { patterns, .. }) => {
                let mut stars = patterns.iter().filter(|pattern| pattern.is_match_star());
                stars.next();
                for extra in stars {
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, extra) {
                        builder.into_diagnostic(
                            "A sequence pattern can have at most one starred capture",
                        );
                    }
                }
                for pattern in patterns {
                    if !pattern.is_match_star() {
                        self.report_invalid_match_type_pattern(pattern);
                    }
                }
            }
            ast::Pattern::MatchOr(ast::PatternMatchOr { patterns, .. })
            | ast::Pattern::MatchAnd(ast::PatternMatchAnd { patterns, .. }) => {
                for pattern in patterns {
                    self.report_invalid_match_type_pattern(pattern);
                }
            }
            ast::Pattern::MatchAs(ast::PatternMatchAs { pattern, .. }) => {
                if let Some(pattern) = pattern.as_deref() {
                    self.report_invalid_match_type_pattern(pattern);
                }
            }
            ast::Pattern::MatchValue(ast::PatternMatchValue { value, .. }) => {
                if literal_pattern_type(self.db(), value).is_none()
                    && let Some(builder) =
                        self.context.report_lint(&INVALID_TYPE_FORM, value.as_ref())
                {
                    builder.into_diagnostic(
                        "A match type's `case` value pattern must be a literal type",
                    );
                }
            }
            ast::Pattern::MatchSingleton(_) => {}
        }
    }

    /// basedpython: reports the two ways a `case` pattern can bind names inconsistently.
    ///
    /// Both are errors python's own parser rejects, which ruff's does not, so they reach
    /// inference. Left unreported they would be silent: a duplicate has to pick one of the
    /// two bindings, and an alternative missing a name leaves the body naming a capture that
    /// was never bound. Evaluation refuses to answer either way, so the diagnostic is what
    /// tells the author which one it is.
    fn report_inconsistent_match_type_captures(&mut self, pattern: &ast::Pattern) {
        let mut seen: Vec<&ast::Identifier> = Vec::new();
        self.report_duplicate_match_type_captures(pattern, &mut seen);
        self.report_uneven_match_type_alternatives(pattern);
    }

    /// Reports a name captured more than once by the same pattern (`case (A, A)`).
    fn report_duplicate_match_type_captures<'p>(
        &mut self,
        pattern: &'p ast::Pattern,
        seen: &mut Vec<&'p ast::Identifier>,
    ) {
        // an or-pattern's alternatives are exclusive, so each starts from the names bound
        // before it rather than from the ones its siblings bound
        if let ast::Pattern::MatchOr(ast::PatternMatchOr { patterns, .. }) = pattern {
            let outer = seen.len();
            for alternative in patterns {
                seen.truncate(outer);
                self.report_duplicate_match_type_captures(alternative, seen);
            }
            seen.truncate(outer);
            return;
        }

        for name in match_type_pattern_captures(pattern) {
            if seen.iter().any(|earlier| earlier.id == name.id) {
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, name) {
                    builder.into_diagnostic(format_args!(
                        "Multiple assignments to name `{}` in a match type's `case` pattern",
                        name.id,
                    ));
                }
            } else {
                seen.push(name);
            }
        }
    }

    /// Reports an or-pattern whose alternatives do not all bind the same names.
    fn report_uneven_match_type_alternatives(&mut self, pattern: &ast::Pattern) {
        if let ast::Pattern::MatchOr(ast::PatternMatchOr { patterns, .. }) = pattern
            && let [first, rest @ ..] = patterns.as_slice()
        {
            let expected: Vec<&ast::name::Name> = match_type_pattern_captures(first)
                .map(|name| &name.id)
                .collect();
            for alternative in rest {
                let bound: Vec<&ast::name::Name> = match_type_pattern_captures(alternative)
                    .map(|name| &name.id)
                    .collect();
                let mut missing = expected
                    .iter()
                    .filter(|name| !bound.contains(*name))
                    .chain(bound.iter().filter(|name| !expected.contains(*name)))
                    .peekable();
                if missing.peek().is_some()
                    && let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, alternative)
                {
                    let names = missing
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    builder.into_diagnostic(format_args!(
                        "Alternative patterns of a match type's `case` must all bind the \
                         same names; not bound by every alternative: {names}"
                    ));
                }
            }
        }

        for nested in match_type_subpatterns(pattern) {
            self.report_uneven_match_type_alternatives(nested);
        }
    }

    /// If the current scope is a method inside an enclosing class,
    /// return `Some(class)` where `class` represents the enclosing class.
    ///
    /// If the current scope is not a method inside an enclosing class,
    /// return `None`.
    ///
    /// Note that this method will only return `Some` if the immediate parent scope
    /// is a class scope OR the immediate parent scope is an annotation scope
    /// and the grandparent scope is a class scope. This means it has different
    /// behaviour to the [`super::nearest_enclosing_class`] function.
    fn class_context_of_current_method(&self) -> Option<ClassType<'db>> {
        let current_scope_id = self.scope().file_scope_id(self.db());
        let class_definition = self.index.class_definition_of_method(current_scope_id)?;
        original_class_type(self.db(), class_definition)
            .map(|class_literal| class_literal.default_specialization(self.db()))
    }

    /// Report an undeclared protocol attribute written through a method receiver.
    ///
    /// The instance or class receiver may be referenced directly, from an eager nested scope, or
    /// through a capture in a nested function.
    fn report_undeclared_protocol_attribute(&self, target: &ast::ExprAttribute) {
        let db = self.db();
        let Some(receiver) = target.value.as_name_expr() else {
            return;
        };
        let Some(method_scope_id) = self.receiver_method_scope(receiver) else {
            return;
        };
        let Some(protocol) = self
            .index
            .class_definition_of_method(method_scope_id)
            .and_then(|definition| original_class_type(db, definition))
            .map(|class| class.default_specialization(db))
            .and_then(|class| class.into_protocol_class(db))
        else {
            return;
        };
        if protocol.interface(db).includes_member(db, target.attr.id())
            || protocol.has_member_declaration(db, target.attr.id())
        {
            return;
        }

        diagnostic::report_undeclared_protocol_attribute(&self.context, target, protocol);
    }

    /// If the current scope is a (non-lambda) function, return that function's AST node.
    ///
    /// If the current scope is not a function (or it is a lambda function), return `None`.
    fn current_function_definition(&self) -> Option<&ast::StmtFunctionDef> {
        let current_scope_id = self.scope().file_scope_id(self.db());
        let current_scope = self.index.scope(current_scope_id);
        if !current_scope.kind().is_non_lambda_function() {
            return None;
        }
        current_scope
            .node()
            .as_function()
            .map(|node_ref| node_ref.node(self.module()))
    }

    fn function_type(&self, function: &ast::StmtFunctionDef) -> Option<FunctionType<'db>> {
        let definition = self.index.expect_single_definition(function);
        infer_definition_types(self.db(), definition).function_type(definition)
    }

    fn current_function_type(&self) -> Option<FunctionType<'db>> {
        self.function_type(self.current_function_definition()?)
    }

    fn function_decorator_types<'a>(
        &'a self,
        function: &'a ast::StmtFunctionDef,
    ) -> impl Iterator<Item = Type<'db>> + 'a {
        let definition = self.index.expect_single_definition(function);

        let definition_types = infer_definition_types(self.db(), definition);

        function
            .decorator_list
            .iter()
            .map(move |decorator| definition_types.expression_type(&decorator.expression))
    }

    /// Returns `true` if the current scope is the function body scope of a function overload (that
    /// is, the stub declaration decorated with `@overload`, not the implementation), or an
    /// abstract method (decorated with `@abstractmethod`.)
    fn in_function_overload_or_abstractmethod(&self) -> bool {
        let Some(function) = self.current_function_definition() else {
            return false;
        };

        self.function_decorator_types(function)
            .any(|decorator_type| {
                match decorator_type {
                    Type::FunctionLiteral(function) => matches!(
                        function.known(self.db()),
                        Some(KnownFunction::Overload | KnownFunction::AbstractMethod)
                    ),
                    Type::Never => {
                        // In unreachable code, we infer `Never` for decorators like `typing.overload`.
                        // Return `true` here to avoid false positive `invalid-return-type` lints for
                        // `@overload`ed functions without a body in unreachable code.
                        true
                    }
                    Type::Divergent(_) => true,
                    _ => false,
                }
            })
    }

    fn infer_body(&mut self, suite: &[ast::Stmt]) {
        let db = self.db();
        for statement in suite {
            self.infer_maybe_standalone_statement(statement);

            if let ast::Stmt::Expr(ast::StmtExpr {
                range: _,
                node_index: _,
                value,
            }) = statement
            {
                let ty = self.expression_type(value);
                if ty.is_awaitable(self.db()) {
                    // an awaitable that reaches the end of a statement is
                    // `unused-awaitable`'s to report: what went missing is the
                    // `await`, not the use of what it would have produced
                    if !self.is_known_function_call(value)
                        && let Some(builder) =
                            self.context.report_lint(&UNUSED_AWAITABLE, value.as_ref())
                    {
                        builder.into_diagnostic(format_args!(
                            "Object of type `{}` is not awaited",
                            ty.display(db, self.program_environment()),
                        ));
                    }
                } else {
                    self.check_unused_return_value(value, ty);
                }
            }
        }
    }

    fn infer_statement(&mut self, statement: &ast::Stmt) {
        match statement {
            ast::Stmt::FunctionDef(function) => self.infer_function_definition_statement(function),
            ast::Stmt::ClassDef(class) => self.infer_class_definition_statement(class),
            ast::Stmt::Expr(ast::StmtExpr {
                range: _,
                node_index: _,
                value,
            }) => {
                // If this is a call expression, we would have added an `IsNonTerminalCall`
                // constraint, meaning this will be a standalone expression.
                self.infer_maybe_standalone_expression(value, TypeContext::default());
            }
            ast::Stmt::If(if_statement) => self.infer_if_statement(if_statement),
            ast::Stmt::Let(let_statement) => self.infer_let_statement(let_statement),
            ast::Stmt::Try(try_statement) => self.infer_try_statement(try_statement),
            ast::Stmt::With(with_statement) => self.infer_with_statement(with_statement),
            ast::Stmt::Match(match_statement) => self.infer_match_statement(match_statement),
            ast::Stmt::Assign(assign) => self.infer_assignment_statement(assign),
            ast::Stmt::AnnAssign(assign) => self.infer_annotated_assignment_statement(assign),
            ast::Stmt::AugAssign(aug_assign) => {
                self.infer_augmented_assignment_statement(aug_assign);
            }
            ast::Stmt::TypeAlias(type_statement) => self.infer_type_alias_statement(type_statement),
            ast::Stmt::For(for_statement) => self.infer_for_statement(for_statement),
            ast::Stmt::While(while_statement) => self.infer_while_statement(while_statement),
            ast::Stmt::Import(import) => self.infer_import_statement(import),
            ast::Stmt::ImportFrom(import) => self.infer_import_from_statement(import),
            ast::Stmt::Assert(assert_statement) => self.infer_assert_statement(assert_statement),
            ast::Stmt::Raise(raise) => self.infer_raise_statement(raise),
            ast::Stmt::Return(ret) => self.infer_return_statement(ret),
            ast::Stmt::Delete(delete) => self.infer_delete_statement(delete),
            ast::Stmt::Global(global) => self.infer_global_statement(global),
            ast::Stmt::Nonlocal(_)
            | ast::Stmt::Break(_)
            | ast::Stmt::Continue(_)
            | ast::Stmt::Pass(_)
            | ast::Stmt::IpyEscapeCommand(_) => {
                // No-op
            }
        }
    }

    fn infer_definition(&mut self, node: impl Into<DefinitionNodeKey> + std::fmt::Debug + Copy) {
        let definition = self.index.expect_single_definition(node);
        let result = infer_definition_types(self.db(), definition);
        self.extend_definition(definition, result);
    }

    fn infer_type_alias_definition(
        &mut self,
        type_alias: &ast::StmtTypeAlias,
        definition: Definition<'db>,
    ) {
        let alias_name = &type_alias.name.as_name_expr().unwrap().id;

        // Check that no type parameter with a default follows a TypeVarTuple
        // in the type alias's PEP 695 type parameter list.
        if let Some(type_params) = type_alias.type_params.as_deref() {
            post_inference::type_param_validation::check_single_typevar_tuple_pep695(
                &self.context,
                type_params,
                post_inference::type_param_validation::TypeParameterOwner::TypeAlias(alias_name),
            );
            post_inference::type_param_validation::check_no_default_after_typevar_tuple_pep695(
                &self.context,
                type_params,
            );
        }

        let rhs_scope = self
            .index
            .node_scope(NodeWithScopeRef::TypeAlias(type_alias))
            .to_scope_id(self.db(), self.program_file());

        let type_alias_ty =
            Type::KnownInstance(KnownInstanceType::TypeAliasType(TypeAliasType::PEP695(
                PEP695TypeAliasType::new(self.db(), alias_name, rhs_scope, None, None),
            )));

        self.store_expression_type(&type_alias.name, type_alias_ty);

        self.add_declaration_with_binding(
            type_alias.into(),
            definition,
            &DeclaredAndInferredType::are_the_same_type(type_alias_ty),
        );
    }

    fn infer_if_statement(&mut self, if_statement: &ast::StmtIf) {
        let ast::StmtIf {
            range: _,
            node_index: _,
            pattern,
            test,
            body,
            elif_else_clauses,
        } = if_statement;

        self.infer_if_condition(pattern.as_deref(), test);
        self.infer_body(body);

        for clause in elif_else_clauses {
            let ast::ElifElseClause {
                range: _,
                node_index: _,
                pattern,
                test,
                body,
            } = clause;

            if let Some(test) = &test {
                self.infer_if_condition(pattern.as_deref(), test);
            }

            self.infer_body(body);
        }
    }

    /// basedpython `let <pattern> := <subject> [else: ...]`.
    fn infer_let_statement(&mut self, let_statement: &ast::StmtLet) {
        let ast::StmtLet {
            range: _,
            node_index: _,
            pattern,
            value,
            orelse,
        } = let_statement;

        self.infer_standalone_expression(value, TypeContext::default());
        self.infer_match_pattern(pattern);
        self.infer_body(orelse);
        self.check_destructure(pattern);
    }

    /// basedpython: reports a destructuring binder whose pattern may not match
    /// the value it destructures, with nothing to handle the failure.
    ///
    /// The captures are bound unconditionally, so a pattern that does not match
    /// leaves them unbound. A `let` statement can handle that with an `else`
    /// block, but only one that diverges — control falling out of the block
    /// reaches the same unbound captures.
    fn check_destructure(&mut self, pattern: &ast::Pattern) {
        let env = self.program_environment();
        let Some(destructure) = self.index.destructure(NodeKey::from_node(pattern)) else {
            return;
        };

        let use_def = self
            .index
            .use_def_map(self.scope().file_scope_id(self.db()));
        if let Some(after_orelse) = destructure.after_orelse {
            if is_reachable(self.db(), use_def, after_orelse)
                && let Some(builder) = self.context.report_lint(&REFUTABLE_DESTRUCTURING, pattern)
            {
                builder.into_diagnostic(
                    "The `else` block of a `let` has to diverge: \
                     what follows it needs the pattern's captures",
                );
            }
            return;
        }

        if analyze_pattern_predicate(self.db(), destructure.predicate).is_always_true() {
            return;
        }

        let subject_ty = pattern_subject_type(self.db(), destructure.predicate.subject(self.db()));

        // a gradual subject cannot be shown to match *or* not to match, and code
        // that never said what it holds is not what this check is for. `Unknown`
        // is what an unannotated parameter, a bare `list` element and an
        // unresolved import all arrive as
        if subject_ty.has_dynamic(self.db(), env) {
            return;
        }

        if let Some(builder) = self.context.report_lint(&REFUTABLE_DESTRUCTURING, pattern) {
            builder.into_diagnostic(format_args!(
                "This pattern may not match `{}`, which would leave its captures unbound",
                subject_ty.display(self.db(), env),
            ));
        }
    }

    /// Infers the condition of an `if` / `elif` clause. With a pattern the clause
    /// is a basedpython `if let <pattern> := <subject>:` — the subject is matched
    /// against the pattern rather than tested for truthiness
    fn infer_if_condition(&mut self, pattern: Option<&ast::Pattern>, test: &ast::Expr) {
        let env = self.program_environment();
        let test_ty = self.infer_standalone_expression(test, TypeContext::default());

        if let Some(pattern) = pattern {
            self.infer_match_pattern(pattern);
        } else if let Err(err) = test_ty.try_bool(self.db(), env) {
            err.report_diagnostic(&self.context, test);
        } else {
            self.check_condition(test);
        }
    }

    fn infer_try_statement(&mut self, try_statement: &ast::StmtTry) {
        let ast::StmtTry {
            range: _,
            node_index: _,
            body,
            handlers,
            orelse,
            finalbody,
            is_star: _,
        } = try_statement;

        self.infer_body(body);

        for handler in handlers {
            let ast::ExceptHandler::ExceptHandler(handler) = handler;
            let ast::ExceptHandlerExceptHandler {
                type_: handled_exceptions,
                name: symbol_name,
                body,
                range: _,
                node_index: _,
            } = handler;

            // If `symbol_name` is `Some()` and `handled_exceptions` is `None`,
            // it's invalid syntax (something like `except as e:`).
            // However, it's obvious that the user *wanted* `e` to be bound here,
            // so we'll have created a definition in the semantic-index stage anyway.
            if symbol_name.is_some() {
                self.infer_definition(handler);
            } else {
                self.infer_exception(handled_exceptions.as_deref(), try_statement.is_star);
            }

            self.infer_body(body);
        }

        self.infer_body(orelse);
        self.infer_body(finalbody);
    }

    fn infer_with_statement(&mut self, with_statement: &ast::StmtWith) {
        let db = self.db();
        let ast::StmtWith {
            range: _,
            node_index: _,
            is_async,
            items,
            body,
        } = with_statement;
        for item in items {
            let target = item.optional_vars.as_deref();
            if let Some(target) = target {
                self.infer_target(target, &item.context_expr, &|builder, tcx| {
                    // TODO: `infer_with_statement_definition` reports a diagnostic if `ctx_manager_ty` isn't a context manager
                    //  but only if the target is a name. We should report a diagnostic here if the target isn't a name:
                    //  `with not_context_manager as a.x: ...
                    builder
                        .infer_standalone_expression(&item.context_expr, tcx)
                        .enter(db, builder.program_environment())
                });
            } else {
                // Call into the context expression inference to validate that it evaluates
                // to a valid context manager.
                let context_expression_ty =
                    self.infer_expression(&item.context_expr, TypeContext::default());
                self.infer_context_expression(&item.context_expr, context_expression_ty, *is_async);
                self.infer_optional_expression(target, TypeContext::default());
            }

            // basedpython: the item destructures the value it binds
            if let Some(pattern) = item.pattern.as_deref() {
                self.infer_match_pattern(pattern);
                self.check_destructure(pattern);
            }
        }

        self.infer_body(body);
    }

    fn infer_with_item_definition(
        &mut self,
        with_item: &WithItemDefinitionKind<'db>,
        definition: Definition<'db>,
    ) {
        let context_expr = with_item.context_expr(self.module());
        let target = with_item.target(self.module());

        let target_ty = match with_item.target_kind() {
            TargetKind::Sequence(unpack_position, unpack) => {
                let unpacked = infer_unpack_types(self.db(), unpack);
                if unpack_position == UnpackPosition::First {
                    self.context.extend(unpacked.diagnostics());
                }
                unpacked.expression_type(target)
            }
            TargetKind::Single => {
                let context_expr_ty =
                    self.infer_standalone_expression(context_expr, TypeContext::default());
                self.infer_context_expression(context_expr, context_expr_ty, with_item.is_async())
            }
        };

        self.store_expression_type(target, target_ty);
        self.add_binding(target.into(), definition)
            .insert(self, target_ty);
    }

    /// Infers the type of a context expression (`with expr`) and returns the target's type
    ///
    /// Returns [`Type::unknown`] if the context expression doesn't implement the context manager protocol.
    ///
    /// ## Terminology
    /// See [PEP343](https://peps.python.org/pep-0343/#standard-terminology).
    fn infer_context_expression(
        &mut self,
        context_expression: &ast::Expr,
        context_expression_type: Type<'db>,
        is_async: bool,
    ) -> Type<'db> {
        let db = self.db();
        let eval_mode = if is_async {
            EvaluationMode::Async
        } else {
            EvaluationMode::Sync
        };

        let env = self.program_environment();
        context_expression_type
            .try_enter_with_mode(db, env, eval_mode)
            .unwrap_or_else(|err| {
                err.report_diagnostic(
                    &self.context,
                    context_expression_type,
                    context_expression.into(),
                );
                err.fallback_enter_type(db, env)
            })
    }

    fn infer_exception(&mut self, node: Option<&ast::Expr>, is_star: bool) -> Type<'db> {
        let db = self.db();
        // If there is no handled exception, it's invalid syntax;
        // a diagnostic will have already been emitted
        let node_ty = node.map_or(Type::unknown(), |ty| {
            self.infer_expression(ty, TypeContext::default())
        });
        let env = self.program_environment();
        let type_base_exception = KnownClass::BaseException.to_subclass_of(db, env);

        // If it's an `except*` handler, this won't actually be the type of the bound symbol;
        // it will actually be the type of the generic parameters to `BaseExceptionGroup` or `ExceptionGroup`.
        let symbol_ty = if let Some(tuple_spec) = node_ty.tuple_instance_spec(db, env) {
            let mut builder = UnionBuilder::new(db, env);
            let mut invalid_elements = vec![];

            for (index, element) in tuple_spec.iter_element_types(self.db()).enumerate() {
                builder.add_in_place(if element.is_assignable_to(db, env, type_base_exception) {
                    element.to_instance_approximation(db, env).expect(
                        "`Type::to_instance()` should always return `Some()` \
                                if called on a type assignable to `type[BaseException]`",
                    )
                } else {
                    invalid_elements.push((index, element));
                    Type::unknown()
                });
            }

            if !invalid_elements.is_empty()
                && let Some(node) = node
            {
                if let ast::Expr::Tuple(tuple) = node
                    && !tuple.iter().any(ast::Expr::is_starred_expr)
                    && Some(tuple.len()) == tuple_spec.len().into_fixed_length()
                {
                    let invalid_elements = invalid_elements
                        .iter()
                        .map(|(index, ty)| (&tuple.elts[*index], *ty));

                    report_invalid_exception_tuple_caught(
                        &self.context,
                        tuple,
                        node_ty,
                        invalid_elements,
                    );
                } else {
                    report_invalid_exception_caught(&self.context, node, node_ty);
                }
            }

            builder.build()
        } else if let Some(symbol_ty) =
            self.exception_handler_symbol_ty_from_valid_ty(node_ty, type_base_exception)
        {
            symbol_ty
        } else if node_ty.is_assignable_to(
            db,
            env,
            UnionType::from_two_elements(
                db,
                env,
                type_base_exception,
                Type::homogeneous_tuple(db, env, type_base_exception),
            ),
        ) {
            // TODO: Handle valid handler expressions that are opaque to the structural helper
            // above, for example a type variable bounded by the full class-or-tuple union.
            KnownClass::BaseException.to_instance(db, env)
        } else {
            if let Some(node) = node {
                report_invalid_exception_caught(&self.context, node, node_ty);
            }
            Type::unknown()
        };

        if is_star {
            let class =
                if symbol_ty.is_subtype_of(db, env, KnownClass::Exception.to_instance(db, env)) {
                    KnownClass::ExceptionGroup
                } else {
                    KnownClass::BaseExceptionGroup
                };
            class.to_specialized_instance(db, env, &[symbol_ty])
        } else {
            symbol_ty
        }
    }

    fn exception_handler_symbol_ty_from_valid_ty(
        &self,
        ty: Type<'db>,
        type_base_exception: Type<'db>,
    ) -> Option<Type<'db>> {
        let db = self.db();
        let env = self.program_environment();

        if let Some(tuple_spec) = ty.tuple_instance_spec(db, env) {
            // `except (ValueError, TypeError) as e:`
            UnionType::try_from_elements(
                db,
                env,
                tuple_spec.iter_element_types(self.db()).map(|element| {
                    if element.is_assignable_to(db, env, type_base_exception) {
                        Some(element.to_instance_approximation(db, env).expect(
                            "`Type::to_instance()` should always return `Some()` \
                                if called on a type assignable to `type[BaseException]`",
                        ))
                    } else {
                        None
                    }
                }),
            )
        } else if ty.is_assignable_to(db, env, type_base_exception) {
            // `except ValueError as e:`
            Some(ty.to_instance_approximation(db, env).expect(
                "`Type::to_instance()` should always return `Some()` \
                    if called on a type assignable to `type[BaseException]`",
            ))
        } else if ty.is_assignable_to(
            db,
            env,
            Type::homogeneous_tuple(db, env, type_base_exception),
        ) {
            // `except exception_types as e:`, where
            // `exception_types: tuple[type[ValueError], ...]`
            Some(
                ty.tuple_instance_spec(db, env)
                    .and_then(|spec| {
                        let specialization = spec
                            .homogeneous_element_type(db, env)
                            .to_instance_approximation(db, env);

                        debug_assert!(specialization.is_some_and(|specialization_type| {
                            specialization_type.is_assignable_to(
                                db,
                                env,
                                KnownClass::BaseException.to_instance(db, env),
                            )
                        }));

                        specialization
                    })
                    .unwrap_or_else(|| KnownClass::BaseException.to_instance(db, env)),
            )
        } else if let Type::Union(union) = ty {
            // `except exception_types as e:`, where
            // `exception_types: type[ValueError] | tuple[type[ValueError], ...]`
            union.try_map(db, env, |element| {
                self.exception_handler_symbol_ty_from_valid_ty(*element, type_base_exception)
            })
        } else {
            None
        }
    }

    fn infer_except_handler_definition(
        &mut self,
        except_handler_definition: &ExceptHandlerDefinitionKind,
        definition: Definition<'db>,
    ) {
        let symbol_ty = self.infer_exception(
            except_handler_definition.handled_exceptions(self.module()),
            except_handler_definition.is_star(),
        );

        self.add_binding(
            except_handler_definition.node(self.module()).into(),
            definition,
        )
        .insert(self, symbol_ty);
    }

    /// Infer the type for a loop header definition.
    ///
    /// The loop header sees all the bindings that originate in the loop and are visible at a
    /// loop-back edge (either the end of the loop body or a `continue` statement). See `struct
    /// LoopHeader` in the semantic index for more on how all this fits together.
    fn infer_loop_header_definition(
        &mut self,
        loop_header_kind: &LoopHeaderDefinitionKind,
        definition: Definition<'db>,
    ) {
        // This cutoff was chosen by benchmarking real isort to keep loop analysis
        // overhead minimal while preserving diagnostics.
        const MAX_EXACT_LOOP_HEADER_REACHABILITY_NODES: usize = 4096;
        let db = self.db();

        let loop_header = loop_header_reachability(self.db(), definition);
        let use_def = self
            .index
            .use_def_map(self.scope().file_scope_id(self.db()));

        // Loop-header types are an approximation point for loop fixpoint analysis. Inferring the
        // exact union of every visible loop-back binding can recursively force inference of large
        // boolean expressions and explode on real-world loops.
        if use_def.reachability_constraints().used_interiors().len()
            > MAX_EXACT_LOOP_HEADER_REACHABILITY_NODES
        {
            self.bindings.insert(definition, Type::unknown());
            return;
        }

        let place = loop_header_kind.place();
        let env = self.program_environment();
        let mut union = UnionBuilder::new(db, env).recursively_defined(RecursivelyDefined::Yes);

        for reachable_binding in &loop_header.reachable_bindings {
            let binding_ty = binding_type(db, reachable_binding.definition);
            let narrowed_ty = use_def
                .narrowing_evaluator(reachable_binding.narrowing_constraint)
                .narrow(db, env, binding_ty, place);

            union.add_in_place(narrowed_ty);
        }

        self.bindings.insert(definition, union.build());
    }

    fn infer_nested_bindings_definition(
        &mut self,
        nested_bindings_kind: &NestedBindingsDefinitionKind,
        definition: Definition<'db>,
    ) {
        const MAX_EXACT_NESTED_BINDING_REACHABILITY_NODES: usize = 2048;

        let db = self.db();
        let scope_id = definition.file_scope(db);
        let mut binding_sources = nested_bindings_kind
            .visible_binding_sources(self.index, scope_id)
            .peekable();
        if binding_sources.peek().is_some()
            && self
                .index
                .use_def_map(scope_id)
                .reachability_constraints()
                .used_interiors()
                .len()
                > MAX_EXACT_NESTED_BINDING_REACHABILITY_NODES
        {
            // As with loop header definitions above, use a reachability cutoff to avoid excessive
            // perf costs in complicated projects like `isort`.
            self.bindings.insert(definition, Type::unknown());
            return;
        }

        let recursively_defined = match nested_bindings_kind.execution {
            NestedBindingExecution::Lazy => RecursivelyDefined::Yes,
            NestedBindingExecution::Eager => RecursivelyDefined::No,
        };
        let env = self.program_environment();
        let mut union = UnionBuilder::new(db, env).recursively_defined(recursively_defined);
        for bindings in binding_sources {
            if nested_bindings_kind.execution == NestedBindingExecution::Eager {
                // A comprehension can execute repeatedly, so a source that is unreachable in the
                // first modeled iteration may become reachable in a later one. Preserve each
                // source's narrowed type and let the proxy's outer use-def state track boundness.
                for binding in bindings {
                    let DefinitionState::Defined(source) = binding.binding else {
                        continue;
                    };
                    let ty = binding_type(db, source);
                    union.add_in_place(binding.narrowing_constraint.narrow(
                        db,
                        env,
                        ty,
                        source.place(db),
                    ));
                }
                continue;
            }

            let Some(ty) = place_from_bindings_with_reachability_cache(
                db,
                env,
                bindings,
                self.reachability_cache(),
            )
            .place
            .raw_type() else {
                continue;
            };
            union.add_in_place(ty);
        }
        let ty = union.build();
        let ty = match nested_bindings_kind.execution {
            NestedBindingExecution::Lazy => ty,
            NestedBindingExecution::Eager => ty.promote(db, env),
        };
        self.bindings.insert(definition, ty);
    }

    fn infer_match_statement(&mut self, match_statement: &ast::StmtMatch) {
        let db = self.db();
        let ast::StmtMatch {
            range: _,
            node_index: _,
            subject,
            cases,
        } = match_statement;

        self.infer_standalone_expression(subject, TypeContext::default());

        for (index, case) in cases.iter().enumerate() {
            let ast::MatchCase {
                range: _,
                node_index: _,
                body,
                pattern,
                guard,
            } = case;
            self.infer_match_pattern(pattern);
            self.check_capturing_case_names(pattern, index + 1 < cases.len() && guard.is_none());

            if let Some(guard) = guard.as_deref() {
                let guard_ty = self.infer_standalone_expression(guard, TypeContext::default());

                if let Err(err) = guard_ty.try_bool(db, self.program_environment()) {
                    err.report_diagnostic(&self.context, guard);
                } else {
                    self.check_condition(guard);
                }
            }

            self.infer_body(body);
        }
    }

    /// basedpython: report the bare `case A:` names that turned out to capture,
    /// where python's own checks were held back because such a name might have
    /// named an enum member of the subject instead.
    ///
    /// Two of python's rules are at stake, and a capture breaks both: it makes
    /// every later case unreachable, and inside an `or` pattern it binds a name
    /// its sibling alternatives do not.
    fn check_capturing_case_names(&mut self, pattern: &ast::Pattern, cases_follow: bool) {
        // a python file's case names are the captures they look like, and the
        // parser has already reported whatever there was to report about them
        if !self.is_basedpython_file() {
            return;
        }
        let mut captures = Vec::new();
        for_each_subject_level_case_name(pattern, false, &mut |alternative, identifier| {
            let Some(case_name) = self.index.case_name(NodeKey::from_node(identifier)) else {
                return;
            };
            if case_name_pattern_type(self.db(), self.program_environment(), case_name).is_none() {
                captures.push((alternative, identifier, case_name));
            }
        });
        for (alternative, identifier, case_name) in captures {
            if alternative {
                report_capturing_case_name_alternative(&self.context, identifier, case_name);
            } else if cases_follow {
                report_capturing_case_name(&self.context, pattern, case_name);
            }
        }
    }

    fn infer_match_pattern_definition(
        &mut self,
        pattern: &'ast ast::Pattern,
        predicate: PatternPredicate<'db>,
        definition: Definition<'db>,
    ) {
        let ty =
            pattern_success_types(self.db(), predicate).binding_type(definition.place(self.db()));
        self.add_binding(pattern.into(), definition)
            .insert(self, ty);
    }

    fn validate_class_pattern(&mut self, pattern: &ast::PatternMatchClass, cls_ty: Type<'db>) {
        let db = self.db();
        let env = self.program_environment();
        if let Type::SpecialForm(SpecialFormType::CollectionsAbcCallable) = cls_ty {
            if let Some(first_excess_pattern) = pattern.arguments.patterns.first() {
                report_too_many_positional_patterns_for_class_pattern(
                    &self.context,
                    first_excess_pattern,
                    0,
                    pattern.arguments.patterns.len(),
                    "collections.abc.Callable",
                );
            }
            return;
        }

        if let Type::ClassLiteral(class) = cls_ty {
            if class.is_typed_dict(self.db()) {
                report_match_pattern_against_typed_dict(&self.context, &*pattern.cls, class);
                return;
            }
            if let Some(protocol_class) = class.into_protocol_class(self.db())
                && !protocol_class.is_runtime_checkable(self.db())
            {
                report_match_pattern_against_non_runtime_checkable_protocol(
                    &self.context,
                    &*pattern.cls,
                    protocol_class,
                );
                return;
            }

            let positional_patterns = &pattern.arguments.patterns;
            if let [first_positional_pattern, ..] = positional_patterns.as_slice()
                && let Some(result) = class_pattern_positional_result(db, env, class)
            {
                match result {
                    ClassPatternPositionalResult::Limit(limit) => {
                        if let Some(first_excess_pattern) = positional_patterns.get(limit) {
                            report_too_many_positional_patterns_for_class_pattern(
                                &self.context,
                                first_excess_pattern,
                                limit,
                                positional_patterns.len(),
                                cls_ty.display(db, env),
                            );
                        }
                    }
                    ClassPatternPositionalResult::InvalidType(match_args_ty) => {
                        report_invalid_match_args_type(
                            &self.context,
                            first_positional_pattern,
                            match_args_ty,
                            cls_ty,
                        );
                    }
                }
            }
        } else if !cls_ty.is_assignable_to(db, env, KnownClass::Type.to_instance(db, env)) {
            report_invalid_class_match_pattern(&self.context, &*pattern.cls, cls_ty);
        }
    }

    fn infer_match_pattern(&mut self, pattern: &ast::Pattern) {
        // We need to create a standalone expression for each arm of a match statement, since they
        // can introduce constraints on the match subject. (Or more accurately, for the match arm's
        // pattern, since its the pattern that introduces any constraints, not the body.) Ideally,
        // that standalone expression would wrap the match arm's pattern as a whole. But a
        // standalone expression can currently only wrap an ast::Expr, which patterns are not. So,
        // we need to choose an Expr that can “stand in” for the pattern, which we can wrap in a
        // standalone expression.
        //
        // The structural pattern is stored separately on `PatternPredicate`, so analyses that need
        // the complete pattern can inspect its arguments without making them standalone
        // expressions.
        //
        // This function is only called for the top-level pattern of a match arm, and is
        // responsible for inferring the standalone expression for each supported pattern type. It
        // then hands off to `infer_nested_match_pattern` for any subexpressions and subpatterns,
        // where we do NOT have any additional standalone expressions to infer through.
        //
        match pattern {
            ast::Pattern::MatchValue(match_value) => {
                self.infer_standalone_expression(&match_value.value, TypeContext::default());
            }
            ast::Pattern::MatchClass(match_class) => {
                let ast::PatternMatchClass {
                    range: _,
                    node_index: _,
                    cls,
                    arguments,
                } = match_class;
                for pattern in &arguments.patterns {
                    self.infer_nested_match_pattern(pattern);
                }
                for keyword in &arguments.keywords {
                    self.infer_nested_match_pattern(&keyword.pattern);
                }
                let cls_ty = self.infer_standalone_expression(cls, TypeContext::default());
                self.validate_class_pattern(match_class, cls_ty);
            }
            ast::Pattern::MatchOr(ast::PatternMatchOr { patterns, .. })
            | ast::Pattern::MatchAnd(ast::PatternMatchAnd { patterns, .. }) => {
                for pattern in patterns {
                    self.infer_match_pattern(pattern);
                }
            }
            _ => {
                self.infer_nested_match_pattern(pattern);
            }
        }
    }

    fn infer_nested_match_pattern(&mut self, pattern: &ast::Pattern) {
        match pattern {
            ast::Pattern::MatchValue(match_value) => {
                self.infer_maybe_standalone_expression(&match_value.value, TypeContext::default());
            }
            ast::Pattern::MatchSequence(match_sequence) => {
                for pattern in &match_sequence.patterns {
                    self.infer_nested_match_pattern(pattern);
                }
            }
            ast::Pattern::MatchMapping(match_mapping) => {
                let ast::PatternMatchMapping {
                    range: _,
                    node_index: _,
                    keys,
                    patterns,
                    rest,
                } = match_mapping;
                for key in keys {
                    self.infer_maybe_standalone_expression(key, TypeContext::default());
                }
                for pattern in patterns {
                    self.infer_nested_match_pattern(pattern);
                }
                if let Some(rest) = rest {
                    self.infer_definition(rest);
                }
            }
            ast::Pattern::MatchClass(match_class) => {
                let ast::PatternMatchClass {
                    range: _,
                    node_index: _,
                    cls,
                    arguments,
                } = match_class;
                for pattern in &arguments.patterns {
                    self.infer_nested_match_pattern(pattern);
                }
                for keyword in &arguments.keywords {
                    self.infer_nested_match_pattern(&keyword.pattern);
                }
                let cls_ty = self.infer_maybe_standalone_expression(cls, TypeContext::default());
                self.validate_class_pattern(match_class, cls_ty);
            }
            ast::Pattern::MatchAs(match_as) => {
                if let Some(pattern) = &match_as.pattern {
                    self.infer_nested_match_pattern(pattern);
                }
                if let Some(name) = &match_as.name {
                    self.infer_definition(name);
                }
            }
            ast::Pattern::MatchOr(ast::PatternMatchOr { patterns, .. })
            | ast::Pattern::MatchAnd(ast::PatternMatchAnd { patterns, .. }) => {
                for pattern in patterns {
                    self.infer_nested_match_pattern(pattern);
                }
            }
            ast::Pattern::MatchStar(match_star) => {
                if let Some(name) = &match_star.name {
                    self.infer_definition(name);
                }
            }
            ast::Pattern::MatchSingleton(_) => {}
        }
    }

    /// basedpython: reports a variable that this scope binds without ever declaring
    /// it with a keyword.
    ///
    /// A class body is left alone: `x: int` there declares a field, which is how a
    /// dataclass or a protocol is written, and `class x = 1` already has a keyword
    /// of its own for the class-variable case.
    fn report_implicit_declaration(&self, target: &ast::Expr) {
        let ast::Expr::Name(name) = target else {
            return;
        };
        if !self.is_basedpython_file() || self.in_stub() {
            return;
        }
        let db = self.db();
        let scope = self.scope().file_scope_id(db);
        if self.index.scope(scope).kind().is_class() {
            return;
        }
        // a bare assignment in a trailing lambda block writes the block receiver's
        // member, so it declares no variable to begin with
        if let NodeWithScopeKind::Function(function) = self.index.scope(scope).node()
            && function.node(self.module()).is_trailing_lambda
        {
            return;
        }
        let place_table = self.index.place_table(scope);
        let Some(symbol_id) = place_table.symbol_id(&name.id) else {
            return;
        };
        let symbol = place_table.symbol(symbol_id);
        // a `global` or `nonlocal` name is declared by the scope that owns it, not
        // by the one writing to it here
        if symbol.is_keyword_declared() || symbol.is_global() || symbol.is_nonlocal() {
            return;
        }
        let Some(builder) = self.context.report_lint(&IMPLICIT_DECLARATION, name) else {
            return;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "`{id}` is assigned without being declared",
            id = name.id
        ));
        diagnostic.info(format_args!(
            "declare it with `let {id} = ...` if it never changes, or `var {id} = ...` if it does",
            id = name.id
        ));
    }

    fn infer_assignment_statement(&mut self, assignment: &ast::StmtAssign) {
        let ast::StmtAssign {
            range: _,
            node_index: _,
            targets,
            value,
            decorator_list: _,
        } = assignment;

        for target in targets {
            self.report_implicit_declaration(target);

            if let Some(unpack) = self.index.try_unpack(target) {
                // Infer the standalone expression here to include its diagnostics in this region.
                self.infer_standalone_expression(value, TypeContext::default());

                let unpacked = infer_unpack_types(self.db(), unpack);
                self.context.extend(unpacked.diagnostics());
                self.infer_unpacked_assignment_target(target, value, unpacked);
            } else {
                self.infer_target(target, value, &|builder, tcx| {
                    builder.infer_standalone_expression(value, tcx)
                });
            }
        }
    }

    fn infer_unpacked_assignment_target(
        &mut self,
        target: &ast::Expr,
        value: &ast::Expr,
        unpacked: &UnpackResult<'db>,
    ) {
        match target {
            ast::Expr::Starred(ast::ExprStarred { value: target, .. }) => {
                self.infer_unpacked_assignment_target(target, value, unpacked);
            }
            ast::Expr::List(ast::ExprList { elts, .. })
            | ast::Expr::Tuple(ast::ExprTuple { elts, .. }) => {
                for target in elts {
                    self.infer_unpacked_assignment_target(target, value, unpacked);
                }
            }
            _ => {
                let assigned_ty = unpacked.expression_type(target);
                self.infer_target_impl(target, value, Some(&|_, _| assigned_ty));
            }
        }
    }

    /// Infer the (definition) types involved in a `target` expression.
    ///
    /// This is used for assignment statements, for statements, etc. with a single or multiple
    /// targets (unpacking). If `target` is an attribute expression, we check that the assignment
    /// is valid. For 'target's that are definitions, this check happens elsewhere.
    ///
    /// The `infer_value_expr` function is used to infer the type of the `value` expression which
    /// are not `Name` expressions. The returned type is the one that is eventually assigned to the
    /// `target`.
    fn infer_target(
        &mut self,
        target: &ast::Expr,
        value: &ast::Expr,
        infer_value_expr: &dyn Fn(&mut Self, TypeContext<'db>) -> Type<'db>,
    ) {
        match target {
            ast::Expr::Name(_) => {
                self.infer_target_impl(target, value, None);
            }

            _ => self.infer_target_impl(target, value, Some(&infer_value_expr)),
        }
    }

    /// Returns `true` if `property_ty` is a property whose deleter returns `Never`/`NoReturn`
    /// when called for deletion on `object_ty`.
    fn property_deleter_returns_never(&self, property_ty: Type<'db>, object_ty: Type<'db>) -> bool {
        let env = self.program_environment();
        let db = self.db();
        property_ty.as_property_instance().is_some_and(|property| {
            property.deleter(db).is_some_and(|deleter| {
                match deleter.try_call(db, env, &CallArguments::positional([object_ty])) {
                    Ok(result) => result.return_type(db, env).is_never(),
                    Err(err) => err.return_type(db, env).is_never(),
                }
            })
        })
    }

    fn validate_attribute_deletion(
        &mut self,
        target: &ast::ExprAttribute,
        object_ty: Type<'db>,
        attribute: &str,
        emit_diagnostics: bool,
    ) -> bool {
        let env = self.program_environment();
        let db = self.db();

        match object_ty {
            // parameter-only marker; behaves as the type a body sees (bound of `Key`)
            Type::Overlapping(overlapping) => self.validate_attribute_deletion(
                target,
                overlapping.value_type(db, env),
                attribute,
                emit_diagnostics,
            ),
            Type::Restricted(restricted) => self.validate_attribute_deletion(
                target,
                restricted.value_type(db),
                attribute,
                emit_diagnostics,
            ),
            Type::Deferred(deferred) => self.validate_attribute_deletion(
                target,
                deferred.reduced(db, env),
                attribute,
                emit_diagnostics,
            ),
            Type::Union(union) => {
                for element_ty in union.elements(db) {
                    if !self.validate_attribute_deletion(
                        target,
                        *element_ty,
                        attribute,
                        emit_diagnostics,
                    ) {
                        return false;
                    }
                }
                true
            }

            Type::Intersection(intersection) => {
                let positive = intersection.positive(db);
                if positive.iter().any(|element_ty| {
                    self.validate_attribute_deletion(target, *element_ty, attribute, false)
                }) {
                    true
                } else {
                    if emit_diagnostics && let Some(element_ty) = positive.first() {
                        self.validate_attribute_deletion(target, *element_ty, attribute, true);
                    }
                    false
                }
            }

            // Deletion succeeds if it succeeds for some materialization, the same any-arm rule
            // as an intersection.
            Type::UnsafeUnion(unsafe_union) => {
                let elements = unsafe_union.elements(db);
                if elements.iter().any(|element_ty| {
                    self.validate_attribute_deletion(target, *element_ty, attribute, false)
                }) {
                    true
                } else {
                    if emit_diagnostics && let Some(element_ty) = elements.first() {
                        self.validate_attribute_deletion(target, *element_ty, attribute, true);
                    }
                    false
                }
            }

            Type::EnumComplement(complement) => self.validate_attribute_deletion(
                target,
                complement.remaining_literal_union(db, env),
                attribute,
                emit_diagnostics,
            ),

            // Type aliases need their own arm so aliased unions and intersections reuse the
            // specialized handling above. `NewType` instances don't: dunder lookup and attribute
            // fallback already delegate through the concrete base type when needed.
            Type::TypeAlias(alias) => self.validate_attribute_deletion(
                target,
                alias.value_type(db),
                attribute,
                emit_diagnostics,
            ),

            Type::NominalInstance(..)
            | Type::ProtocolInstance(_)
            | Type::LiteralValue(..)
            | Type::SpecialForm(..)
            | Type::ClassLiteral(..)
            | Type::GenericAlias(..)
            | Type::SubclassOf(..)
            | Type::KnownInstance(..)
            | Type::PropertyInstance(..)
            | Type::FunctionLiteral(..)
            | Type::Callable(..)
            | Type::BoundMethod(_)
            | Type::KnownBoundMethod(_)
            | Type::WrapperDescriptor(_)
            | Type::DataclassDecorator(_)
            | Type::DataclassTransformer(_)
            | Type::TypeVar(..)
            | Type::AlwaysTruthy
            | Type::AlwaysFalsy
            | Type::TypeIs(_)
            | Type::TypeGuard(_)
            | Type::TypeForm(_)
            | Type::TypedDict(_)
            | Type::NewTypeInstance(_) => {
                let frozen_dataclass_dispatch = object_ty
                    .nominal_class(db, env)
                    .and_then(|class| class.static_class_literal(db))
                    .and_then(|(class, specialization)| {
                        class.inherited_frozen_dataclass_dispatch(
                            db,
                            specialization,
                            "__delattr__",
                            attribute,
                        )
                    });

                let delattr_receiver = frozen_dataclass_dispatch
                    .map_or(object_ty, |dispatch| dispatch.receiver(db, env, object_ty));

                let mut delattr_arguments =
                    CallArguments::positional([Type::string_literal(db, attribute)]);
                let delattr_dunder_call_result = if matches!(delattr_receiver, Type::BoundSuper(_))
                {
                    match delattr_receiver
                        .member_lookup_with_policy(
                            db,
                            env,
                            "__delattr__",
                            MemberLookupPolicy::MRO_NO_OBJECT_FALLBACK,
                        )
                        .place
                    {
                        Place::Defined(DefinedPlace {
                            ty: delattr,
                            definedness,
                            provenance,
                            ..
                        }) => match delattr.try_call(db, env, &delattr_arguments) {
                            Ok(bindings) if definedness == Definedness::PossiblyUndefined => {
                                Err(CallDunderError::PossiblyUnbound {
                                    bindings: Box::new(bindings),
                                    unbound_on: None,
                                })
                            }
                            Ok(bindings) => Ok(bindings),
                            Err(CallError(kind, bindings)) => {
                                Err(CallDunderError::CallError(kind, bindings, provenance))
                            }
                        },
                        Place::Undefined => Err(CallDunderError::MethodNotAvailable),
                    }
                } else {
                    delattr_receiver.try_call_dunder_with_policy(
                        db,
                        env,
                        "__delattr__",
                        &mut delattr_arguments,
                        TypeContext::default(),
                        MemberLookupPolicy::MRO_NO_OBJECT_FALLBACK,
                    )
                };

                let returns_never = matches!(
                    frozen_dataclass_dispatch,
                    Some(FrozenDataclassDispatch::FrozenField)
                ) || match &delattr_dunder_call_result {
                    Ok(result) => result.return_type(db, env).is_never(),
                    Err(err) => err.return_type(db, env).is_some_and(|ty| ty.is_never()),
                };
                if returns_never {
                    if emit_diagnostics
                        && let Some(builder) = self.context.report_lint(&INVALID_ASSIGNMENT, target)
                    {
                        builder.into_diagnostic(format_args!(
                            "Cannot delete attribute `{attribute}` on type `{}` \
                             whose `__delattr__` method returns `Never`/`NoReturn`",
                            object_ty.display(db, env),
                        ));
                    }
                    return false;
                }

                match delattr_dunder_call_result {
                    Ok(_) | Err(CallDunderError::PossiblyUnbound { .. })
                        if !matches!(
                            frozen_dataclass_dispatch,
                            Some(FrozenDataclassDispatch::Delegate(_))
                        ) =>
                    {
                        if self.validate_final_attribute_deletion(
                            target,
                            object_ty,
                            attribute,
                            emit_diagnostics,
                        ) {
                            return false;
                        }
                        return true;
                    }
                    Ok(_) | Err(CallDunderError::PossiblyUnbound { .. }) => {}
                    Err(CallDunderError::CallError(kind, _bindings, _)) => {
                        if emit_diagnostics {
                            report_bad_dunder_delattr_call(
                                &self.context,
                                attribute,
                                object_ty,
                                target,
                                kind == CallErrorKind::BindingError,
                            );
                        }
                        return false;
                    }
                    Err(CallDunderError::MethodNotAvailable) => {}
                }

                if self.validate_final_attribute_deletion(
                    target,
                    object_ty,
                    attribute,
                    emit_diagnostics,
                ) {
                    return false;
                }

                if let Some(PlaceAndQualifiers {
                    place:
                        Place::Defined(DefinedPlace {
                            ty: attr_ty,
                            definedness: Definedness::AlwaysDefined,
                            ..
                        }),
                    ..
                }) = assignment_attribute_members(db, env, object_ty, attribute)
                    .and_then(AssignmentAttributeMembers::type_member)
                {
                    let attr_ty = attr_ty.bind_self_typevars(db, env, object_ty);
                    let delete_dunder_call_result = attr_ty.try_call_dunder(
                        db,
                        env,
                        "__delete__",
                        CallArguments::positional([object_ty]),
                        TypeContext::default(),
                    );

                    // `Never` supports arbitrary operations only because there can be no runtime
                    // value to mutate; it is not a concrete descriptor with a terminal deleter.
                    let deleter_returns_never = !attr_ty.is_never()
                        && match &delete_dunder_call_result {
                            Ok(bindings) => bindings.return_type(db, env).is_never(),
                            Err(error) => {
                                error.return_type(db, env).is_some_and(|ty| ty.is_never())
                            }
                        };
                    if deleter_returns_never
                        || self.property_deleter_returns_never(attr_ty, object_ty)
                    {
                        if emit_diagnostics
                            && let Some(builder) =
                                self.context.report_lint(&INVALID_ASSIGNMENT, target)
                        {
                            builder.into_diagnostic(format_args!(
                                "Cannot delete attribute `{attribute}` on type `{}` \
                                 whose `__delete__` method returns `Never`/`NoReturn`",
                                object_ty.display(db, env),
                            ));
                        }
                        return false;
                    }

                    match delete_dunder_call_result {
                        Ok(_) | Err(CallDunderError::PossiblyUnbound { .. }) => return true,
                        Err(CallDunderError::CallError(kind, bindings, _)) => {
                            if emit_diagnostics {
                                let failure = CallError(kind, bindings);
                                report_bad_dunder_delete_call(
                                    &self.context,
                                    &failure,
                                    attribute,
                                    object_ty,
                                    target,
                                );
                            }
                            return false;
                        }
                        Err(CallDunderError::MethodNotAvailable) => {}
                    }
                }

                true
            }

            Type::Dynamic(..)
            | Type::Divergent(_)
            | Type::Never
            | Type::ModuleLiteral(..)
            | Type::BoundSuper(..) => true,
        }
    }

    #[expect(clippy::type_complexity)]
    fn infer_target_impl(
        &mut self,
        target: &ast::Expr,
        value: &ast::Expr,
        infer_assigned_ty: Option<&dyn Fn(&mut Self, TypeContext<'db>) -> Type<'db>>,
    ) {
        let db = self.db();
        match target {
            ast::Expr::Name(name) => {
                if let Some(infer_assigned_ty) = infer_assigned_ty {
                    infer_assigned_ty(self, TypeContext::default());
                }

                self.infer_definition(name);
                self.validate_receiver_member_write(name, value);
            }
            ast::Expr::Starred(ast::ExprStarred {
                value: starred_value,
                ..
            }) => {
                self.infer_target_impl(starred_value, value, infer_assigned_ty);
            }
            ast::Expr::List(ast::ExprList { elts, .. })
            | ast::Expr::Tuple(ast::ExprTuple { elts, .. }) => {
                let assigned_ty = infer_assigned_ty.map(|f| f(self, TypeContext::default()));

                if let Some(tuple_spec) = assigned_ty
                    .and_then(|ty| ty.tuple_instance_spec(db, self.program_environment()))
                {
                    let assigned_tys = tuple_spec.iter_element_types(self.db()).collect::<Vec<_>>();

                    for (i, element) in elts.iter().enumerate() {
                        match assigned_tys.get(i).copied() {
                            None => self.infer_target_impl(element, value, None),
                            Some(ty) => self.infer_target_impl(element, value, Some(&|_, _| ty)),
                        }
                    }
                } else {
                    for element in elts {
                        self.infer_target_impl(element, value, None);
                    }
                }
            }
            ast::Expr::Attribute(
                attr_expr @ ast::ExprAttribute {
                    value: object,
                    ctx: ExprContext::Store,
                    attr,
                    ..
                },
            ) => {
                let object_ty = self.infer_expression(object, TypeContext::default());
                self.report_undeclared_protocol_attribute(attr_expr);

                if let Some(infer_assigned_ty) = infer_assigned_ty {
                    let infer_assigned_ty = &mut |builder: &mut Self, tcx| {
                        let assigned_ty = infer_assigned_ty(builder, tcx);
                        builder.store_expression_type(target, assigned_ty);
                        assigned_ty
                    };

                    self.validate_attribute_assignment(
                        attr_expr,
                        value,
                        object_ty,
                        attr.id(),
                        infer_assigned_ty,
                        true,
                    );
                }
            }
            ast::Expr::Subscript(subscript_expr) => {
                if let Some(infer_assigned_ty) = infer_assigned_ty {
                    let object_ty =
                        self.infer_expression(&subscript_expr.value, TypeContext::default());
                    let mut infer_slice_ty = |builder: &mut Self, tcx| {
                        builder.infer_expression(&subscript_expr.slice, tcx)
                    };
                    let infer_assigned_ty = &mut |builder: &mut Self, tcx| {
                        let assigned_ty = infer_assigned_ty(builder, tcx);
                        builder.store_expression_type(target, assigned_ty);
                        assigned_ty
                    };

                    self.validate_subscript_assignment(
                        subscript_expr,
                        value,
                        object_ty,
                        &mut infer_slice_ty,
                        infer_assigned_ty,
                    );
                }
            }

            // TODO: Remove this once we handle all possible assignment targets.
            _ => {
                if let Some(infer_assigned_ty) = infer_assigned_ty {
                    infer_assigned_ty(self, TypeContext::default());
                }

                self.infer_expression(target, TypeContext::default());
            }
        }
    }

    fn infer_assignment_definition(
        &mut self,
        assignment: &AssignmentDefinitionKind<'db>,
        definition: Definition<'db>,
    ) {
        let target = assignment.target(self.module());

        let add = self.add_binding(target.into(), definition);
        let target_ty =
            self.infer_assignment_definition_impl(assignment, definition, add.type_context());
        self.store_expression_type(target, target_ty);
        add.insert(self, target_ty);
        self.check_unannotated_model_field(target, assignment.value(self.module()));
        self.check_django_field_name_list(target, assignment.value(self.module()));
    }

    /// Pydantic requires every field to be annotated. An unannotated class-body
    /// assignment of a field specifier (`name = Field(...)`) is not collected as
    /// a field and raises `PydanticUserError` when the model class is created, so
    /// report it. (Dataclasses tolerate this — it becomes a plain class
    /// attribute — so the check is gated to pydantic models.)
    fn check_unannotated_model_field(&mut self, target: &ast::Expr, value: &ast::Expr) {
        let ast::Expr::Name(name) = target else {
            return;
        };
        // the r.h.s. must be a call to pydantic's `Field` specifier. (Without an
        // annotation the field-specifier evaluation does not run, so the call's
        // inferred type is just its default — detect it by the callee instead.)
        let ast::Expr::Call(call) = value else {
            return;
        };
        let is_pydantic_field = self
            .try_expression_type(&call.func)
            .and_then(Type::as_function_literal)
            .and_then(|function| function.known(self.db()))
            .is_some_and(|known| known == KnownFunction::PydanticField);
        if !is_pydantic_field {
            return;
        }
        let db = self.db();
        let enclosing = self.index.scope(self.scope().file_scope_id(db));
        let Some(class_node) = enclosing.node().as_class() else {
            return;
        };
        let class_definition = self.index.expect_single_definition(class_node);
        let Some(class_literal) =
            original_class_type(db, class_definition).and_then(ClassLiteral::as_static)
        else {
            return;
        };
        if !pydantic::is_model(db, class_literal) {
            return;
        }
        if let Some(builder) = self.context.report_lint(&UNANNOTATED_MODEL_FIELD, target) {
            builder.into_diagnostic(format_args!(
                "Field `{}` needs a type annotation to become a pydantic model field",
                name.id
            ));
        }
    }

    /// The class whose body `scope` is, or `None` when `scope` is not a class body.
    fn class_of_body_scope(&self, scope: FileScopeId) -> Option<StaticClassLiteral<'db>> {
        let class_node = self.index.scope(scope).node().as_class()?;
        let definition = self.index.expect_single_definition(class_node);
        original_class_type(self.db(), definition)?.as_static()
    }

    /// The class lexically enclosing the class whose body `scope` is — for a
    /// nested `Meta`, the model / serializer / form that declares it. A generic
    /// outer class interposes its type-parameter scope, so skip that hop.
    fn class_enclosing_body_scope(&self, scope: FileScopeId) -> Option<StaticClassLiteral<'db>> {
        let mut parent = self.index.parent_scope_id(scope)?;
        if self.index.scope(parent).kind() == ScopeKind::TypeParams {
            parent = self.index.parent_scope_id(parent)?;
        }
        self.class_of_body_scope(parent)
    }

    /// Validate a class-body assignment of a list of django model field paths:
    /// a model's `Meta.ordering`, and a drf view's `ordering_fields`,
    /// `filterset_fields` and `search_fields`.
    ///
    /// Every one of these is a list of strings the stubs type as plain `str`,
    /// so a typo is invisible until django resolves it — at import time for
    /// `Meta.ordering` (`models.E015`), and only once a request asks for that
    /// ordering / search / filter for the view attributes.
    ///
    /// Reporting requires the model to be resolved for certain. A class body
    /// that names no model, a `queryset` that doesn't trace to one, and a
    /// non-literal element are all left alone rather than guessed at.
    fn check_django_field_name_list(&mut self, target: &ast::Expr, value: &ast::Expr) {
        let env = self.program_environment();
        let ast::Expr::Name(name) = target else {
            return;
        };
        let db = self.db();
        let scope = self.scope().file_scope_id(db);
        let name = name.id.as_str();

        // `fields` / `exclude` / `ordering` all sit in a nested `Meta`; the
        // drf view attributes sit in the view's own body
        let meta = self
            .class_of_body_scope(scope)
            .filter(|meta| meta.name(db) == "Meta");
        let declaring = meta.and_then(|_| self.class_enclosing_body_scope(scope));

        let resolved = if name == "ordering" {
            // a model's `Meta.ordering`: the model is the class declaring `Meta`
            declaring
                .filter(|model| django::is_model(db, *model))
                .map(|model| (model, django::FieldListKind::Ordering))
        } else if let Some((meta, declaring)) = meta.zip(declaring)
            && let Some(declarer) = django::meta_fields_declarer(db, declaring)
            && declarer.checks(name)
        {
            // a serializer's / form's `Meta.fields`: the model is `Meta.model`
            django::meta_model(db, meta)
                .map(|model| (model, django::FieldListKind::MetaFields { declaring }))
        } else {
            django::view_field_list_kind(name).and_then(|kind| {
                let view = self.class_of_body_scope(scope)?;
                Some((django::drf_view_model(db, view)?, kind))
            })
        };
        let Some((model, kind)) = resolved else {
            return;
        };

        // a list this can't read exhaustively is left alone entirely
        let Some(entries) = django::literal_field_list_entries(value) else {
            return;
        };
        for (entry, range) in entries {
            if let django::FieldResolution::Unknown { model, segment } =
                kind.resolve(db, env, model, entry)
                && let Some(builder) = self.context.report_lint(&INVALID_FIELD_LOOKUP, range)
            {
                builder.into_diagnostic(format_args!(
                    "Model `{model}` has no field `{segment}` (in `{name}`)"
                ));
            }
        }
    }

    fn stub_placeholder_binding_type(&self, value: &ast::Expr) -> Option<Type<'db>> {
        if self.in_stub() && value.is_ellipsis_literal_expr() {
            Some(Type::unknown())
        } else {
            None
        }
    }

    fn infer_assignment_definition_impl(
        &mut self,
        assignment: &AssignmentDefinitionKind<'db>,
        definition: Definition<'db>,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let value = assignment.value(self.module());
        let target = assignment.target(self.module());

        let mut target_ty = match assignment.unpack() {
            Some(unpack) => {
                // The assignment statement owns unpacking diagnostics so that targets without a
                // name definition are still checked, and each diagnostic is reported only once.
                let unpacked = infer_unpack_types(self.db(), unpack);
                unpacked.expression_type(target)
            }
            None => {
                // This could be an implicit type alias (OptionalList = list[T] | None). Use the definition
                // of `OptionalList` as the binding context while inferring the RHS (`list[T] | None`), in
                // order to bind `T` to `OptionalList`.
                let previous_typevar_binding_context =
                    self.typevar_binding_context.replace(definition);

                let value_ty = if let Some(standalone_expression) = self.index.try_expression(value)
                {
                    self.infer_standalone_expression_impl(value, standalone_expression, tcx)
                } else if let ast::Expr::Call(call_expr) = value
                    && call_expr.cast_kind.is_none()
                {
                    // If the RHS is not a standalone expression, this is a simple assignment
                    // (single target, no unpackings). That means it's a valid syntactic form
                    // for a legacy TypeVar creation; check for that.
                    let callable_type = self.infer_maybe_standalone_expression(
                        call_expr.func.as_ref(),
                        TypeContext::default(),
                    );

                    let ty = if let Some(namedtuple_kind) =
                        NamedTupleKind::from_type(self.db(), callable_type)
                    {
                        self.infer_namedtuple_call_expression(
                            call_expr,
                            Some(definition),
                            namedtuple_kind,
                        )
                    } else if let Some(typed_dict_module) =
                        TypedDictModule::from_type(self.db(), callable_type)
                    {
                        self.infer_typeddict_call_expression(
                            call_expr,
                            Some(definition),
                            typed_dict_module,
                        )
                    } else if let Some(function) = callable_type.as_function_literal()
                        && function.is_known(self.db(), KnownFunction::NewClass)
                    {
                        self.infer_new_class_call(call_expr, Some(definition))
                    } else if let Some(base_class) =
                        enum_call::enum_functional_call_base(self.db(), callable_type)
                        && let Some(ty) =
                            self.infer_enum_call_expression(call_expr, Some(definition), base_class)
                    {
                        ty
                    } else {
                        match callable_type
                            .as_class_literal()
                            .and_then(|cls| cls.known(self.db()))
                        {
                            Some(
                                typevar_class @ (KnownClass::TypeVar
                                | KnownClass::ExtensionsTypeVar),
                            ) => self.infer_legacy_typevar(
                                target,
                                call_expr,
                                definition,
                                typevar_class,
                            ),
                            Some(
                                paramspec_class @ (KnownClass::ParamSpec
                                | KnownClass::ExtensionsParamSpec),
                            ) => self.infer_legacy_paramspec(
                                target,
                                call_expr,
                                definition,
                                paramspec_class,
                            ),
                            Some(
                                typevartuple_class @ (KnownClass::TypeVarTuple
                                | KnownClass::ExtensionsTypeVarTuple),
                            ) => self.infer_legacy_typevartuple(
                                target,
                                call_expr,
                                definition,
                                typevartuple_class,
                            ),
                            Some(KnownClass::NewType) => {
                                self.infer_newtype_expression(target, call_expr, definition)
                            }
                            Some(KnownClass::Type) => {
                                // Try to extract the dynamic class with definition.
                                // This returns `None` if it's not a three-arg call to `type()`,
                                // signalling that we must fall back to normal call inference.
                                self.infer_builtins_type_call(call_expr, Some(definition))
                            }
                            Some(KnownClass::TypeAliasType) => {
                                self.infer_typealiastype_call(target, call_expr, definition)
                            }
                            Some(KnownClass::Sentinel) => self
                                .infer_sentinel_expression(target, call_expr, definition)
                                .unwrap_or_else(|| {
                                    self.infer_call_expression_impl(call_expr, callable_type, tcx)
                                }),
                            Some(_) | None => {
                                self.infer_fluid_constructor_call(call_expr, callable_type, tcx)
                            }
                        }
                    };

                    let ty = if target.as_name_expr().is_some()
                        && self
                            .index
                            .scope(self.scope().file_scope_id(self.db()))
                            .kind()
                            == ScopeKind::Class
                    {
                        self.apply_desugared_decorator(callable_type, call_expr, ty)
                    } else {
                        ty
                    };

                    // this branch answers a call without going through
                    // `infer_call_expression`, so a method call on a symbolic receiver has to
                    // be kept symbolic here too — otherwise `a = x.foo()` would name a
                    // different value than the `x.foo()` a `return` names
                    let ty = self.basedpython_symbolic_call(call_expr, callable_type, ty);

                    self.store_expression_type(value, ty);
                    ty
                } else {
                    self.infer_expression(value, tcx)
                };

                self.typevar_binding_context = previous_typevar_binding_context;

                // `TYPE_CHECKING` is a special variable that should only be assigned `False`
                // at runtime, but is always considered `True` in type checking.
                // See mdtest/known_constants.md#user-defined-type_checking for details.
                if target.as_name_expr().map(|name| name.id.as_str()) == Some("TYPE_CHECKING") {
                    if !matches!(
                        value.as_boolean_literal_expr(),
                        Some(ast::ExprBooleanLiteral { value: false, .. })
                    ) {
                        report_invalid_type_checking_constant(&self.context, target.into());
                    }
                    Type::bool_literal(true)
                } else {
                    self.stub_placeholder_binding_type(value)
                        .unwrap_or(value_ty)
                }
            }
        };

        // basedpython: a decorator written above the assignment applies to what it
        // binds — `@foo` above `x = 1` binds `foo(1)`
        target_ty = self.infer_binding_decorators(assignment.decorators(self.module()), target_ty);

        if let Some(special_form) = target.as_name_expr().and_then(|name| {
            let db = self.db();
            let importing_file = ImportingFile::File(
                self.file(),
                self.program_environment().resolver_environment(db),
            );
            SpecialFormType::try_from_file_and_name(db, importing_file, &name.id)
        }) {
            target_ty = Type::SpecialForm(special_form);
        }

        target_ty
    }

    fn infer_newtype_expression(
        &mut self,
        target: &ast::Expr,
        call_expr: &ast::ExprCall,
        definition: Definition<'db>,
    ) -> Type<'db> {
        fn error<'db>(
            context: &InferContext<'db, '_>,
            message: impl std::fmt::Display,
            node: impl Ranged,
        ) -> Type<'db> {
            if let Some(builder) = context.report_lint(&INVALID_NEWTYPE, node) {
                builder.into_diagnostic(message);
            }
            Type::unknown()
        }

        let db = self.db();
        let arguments = &call_expr.arguments;

        if !arguments.keywords.is_empty() {
            return error(
                &self.context,
                "Keyword arguments are not supported in `NewType` creation",
                call_expr,
            );
        }

        if let Some(starred) = arguments.args.iter().find(|arg| arg.is_starred_expr()) {
            return error(
                &self.context,
                "Starred arguments are not supported in `NewType` creation",
                starred,
            );
        }

        if arguments.args.len() != 2 {
            return error(
                &self.context,
                format!(
                    "Wrong number of arguments in `NewType` creation: expected 2, found {}",
                    arguments.args.len()
                ),
                call_expr,
            );
        }

        let name_param_ty = self.infer_expression(&arguments.args[0], TypeContext::default());

        let Some(name) = name_param_ty.as_string_literal().map(|name| name.value(db)) else {
            return error(
                &self.context,
                "The first argument to `NewType` must be a string literal",
                call_expr,
            );
        };

        let ast::Expr::Name(ast::ExprName {
            id: target_name, ..
        }) = target
        else {
            return error(
                &self.context,
                "A `NewType` definition must be a simple variable assignment",
                target,
            );
        };

        if name != target_name {
            report_mismatched_type_name(
                &self.context,
                &arguments.args[0],
                "NewType",
                target_name,
                Some(name),
                name_param_ty,
            );
        }

        // Inference of `tp` must be deferred, to avoid cycles.
        self.deferred.insert(definition);

        Type::KnownInstance(KnownInstanceType::NewType(NewType::new(
            db, name, definition, None,
        )))
    }

    fn infer_sentinel_expression(
        &mut self,
        target: &ast::Expr,
        call_expr: &ast::ExprCall,
        definition: Definition<'db>,
    ) -> Option<Type<'db>> {
        if !self.sentinel_definition_scope_is_supported() {
            return None;
        }

        let ast::Expr::Name(ast::ExprName {
            id: target_name, ..
        }) = target
        else {
            return None;
        };

        let ast::Arguments {
            args,
            keywords,
            range: _,
            node_index: _,
        } = &call_expr.arguments;

        if args.iter().any(ast::Expr::is_starred_expr) {
            return None;
        }

        let (name_arg, mut repr_arg) = match &**args {
            [name_arg] => (name_arg, None),
            [name_arg, repr_arg] => (name_arg, Some(repr_arg)),
            _ => return None,
        };

        for keyword in keywords {
            let Some(keyword_name) = &keyword.arg else {
                return None;
            };

            if keyword_name.as_str() != "repr" || repr_arg.is_some() {
                return None;
            }

            repr_arg = Some(&keyword.value);
        }

        if !matches!(name_arg, ast::Expr::StringLiteral(_)) {
            return None;
        }

        let Some(repr_arg) = repr_arg else {
            return Some(Type::KnownInstance(KnownInstanceType::Sentinel(
                SentinelInstance::new(self.db(), target_name, definition),
            )));
        };

        if !matches!(repr_arg, ast::Expr::StringLiteral(_)) && !repr_arg.is_none_literal_expr() {
            return None;
        }

        Some(Type::KnownInstance(KnownInstanceType::Sentinel(
            SentinelInstance::new(self.db(), target_name, definition),
        )))
    }

    fn sentinel_definition_scope_is_supported(&self) -> bool {
        let db = self.db();
        let mut scope_id = self.scope.file_scope_id(db);

        loop {
            let scope = self.index.scope(scope_id);
            match scope.node().scope_kind() {
                ScopeKind::Module => return true,
                ScopeKind::Class => {}
                ScopeKind::Function
                | ScopeKind::Lambda
                | ScopeKind::Comprehension
                | ScopeKind::TypeAlias
                | ScopeKind::TypeParams => return false,
            }

            let Some(parent) = scope.parent() else {
                return false;
            };
            scope_id = parent;
        }
    }

    fn infer_assignment_deferred(&mut self, target: &ast::Expr, value: &'ast ast::Expr) {
        let db = self.db();
        let env = self.program_environment();
        // Infer deferred bounds/constraints/defaults of a legacy TypeVar / ParamSpec / NewType,
        // and field types for functional TypedDict.
        let ast::Expr::Call(ast::ExprCall {
            func, arguments, ..
        }) = value
        else {
            return;
        };
        let func_ty = self
            .try_expression_type(func)
            .unwrap_or_else(|| self.infer_expression(func, TypeContext::default()));
        if func_ty == Type::SpecialForm(SpecialFormType::NamedTuple) {
            // Only the `fields` argument is deferred for `NamedTuple`;
            // other arguments are inferred eagerly.
            self.infer_typing_namedtuple_fields(&arguments.args[1]);
            return;
        }
        let known_class = func_ty
            .as_class_literal()
            .and_then(|cls| cls.known(self.db()));
        match (known_class, self.region) {
            (Some(KnownClass::NewType), _) => {
                self.infer_newtype_assignment_deferred(arguments);
                return;
            }
            (Some(KnownClass::TypeAliasType), InferenceRegion::Deferred(definition)) => {
                self.infer_typealiastype_assignment_deferred(definition, arguments);
                return;
            }
            (Some(KnownClass::Type), InferenceRegion::Deferred(definition)) => {
                self.infer_builtins_type_deferred(definition, value);
                return;
            }
            _ => {}
        }
        if TypedDictModule::from_type(self.db(), func_ty).is_some() {
            self.infer_functional_typeddict_deferred(arguments);
            return;
        }
        if let InferenceRegion::Deferred(definition) = self.region
            && let Some(function) = func_ty.as_function_literal()
            && function.is_known(self.db(), KnownFunction::NewClass)
        {
            self.infer_new_class_deferred(definition, value);
            return;
        }
        let mut constraint_tys = Vec::new();
        for arg in arguments.args.iter().skip(1) {
            let constraint = self.infer_type_expression(arg);
            constraint_tys.push(constraint);

            if constraint.has_typevar_or_typevar_instance(db, env)
                && let Some(builder) = self
                    .context
                    .report_lint(&INVALID_TYPE_VARIABLE_CONSTRAINTS, arg)
            {
                builder.into_diagnostic("TypeVar constraint cannot be generic");
            }
        }
        let mut bound_or_constraints = if !constraint_tys.is_empty() {
            Some(TypeVarBoundOrConstraints::Constraints(
                TypeVarConstraints::new(self.db(), constraint_tys.into_boxed_slice()),
            ))
        } else {
            None
        };
        if let Some(bound) = arguments.find_keyword("bound") {
            let bound_type = self.infer_type_variable_bound(&bound.value);
            bound_or_constraints = Some(TypeVarBoundOrConstraints::UpperBound(bound_type));
        }
        if let Some(default) = arguments.find_keyword("default") {
            if matches!(
                known_class,
                Some(KnownClass::TypeVarTuple | KnownClass::ExtensionsTypeVarTuple)
            ) {
                self.infer_typevartuple_default(&default.value, None);
            } else if matches!(
                known_class,
                Some(KnownClass::ParamSpec | KnownClass::ExtensionsParamSpec)
            ) {
                // Pass `None` for the name: the outer-scope typevar check inside
                // `infer_paramspec_default` is only relevant for PEP 695 type parameter
                // scopes. Legacy ParamSpec definitions live at module/class-body scope,
                // so the check would be a no-op here. Out-of-scope defaults for legacy
                // typevars are instead validated by `check_legacy_typevar_defaults`
                // (for functions) and `report_invalid_typevar_default_reference`
                // (for classes).
                self.infer_paramspec_default(&default.value, None);
            } else {
                let default_ty = self.infer_type_expression(&default.value);
                let bound_or_constraints_node = arguments
                    .find_keyword("bound")
                    .map(|kw| BoundOrConstraintsNodes::Bound(&kw.value))
                    .or_else(|| {
                        if arguments.args.len() < 3 {
                            return None;
                        }
                        Some(BoundOrConstraintsNodes::Constraints(&arguments.args[1..]))
                    });
                self.validate_typevar_default(
                    target.as_name_expr().map(|name| &*name.id),
                    bound_or_constraints,
                    default_ty,
                    &default.value,
                    bound_or_constraints_node,
                );
            }
        }
    }

    // Infer the deferred base type of a NewType.
    fn infer_newtype_assignment_deferred(&mut self, arguments: &ast::Arguments) {
        let db = self.db();
        let env = self.program_environment();
        let inferred = self.infer_type_expression(&arguments.args[1]);

        if inferred.has_typevar_or_typevar_instance(db, env) {
            if let Some(builder) = self
                .context
                .report_lint(&INVALID_NEWTYPE, &arguments.args[1])
            {
                let mut diag = builder.into_diagnostic("invalid base for `typing.NewType`");
                diag.set_primary_annotation_message("A `NewType` base cannot be generic");
            }
            return;
        }

        match inferred {
            Type::NewTypeInstance(_) | Type::NominalInstance(_) => return,
            // There are exactly two union types allowed as bases for NewType: `int | float` and
            // `int | float | complex`. These are allowed because that's what `float` and `complex`
            // expand into in type position. We don't currently ask whether the union was implicit
            // or explicit, so the explicit version is also allowed.
            Type::Union(union_ty) => {
                if let Some(KnownUnion::Float | KnownUnion::Complex) = union_ty.known(self.db()) {
                    return;
                }
            }
            // `Unknown` is likely to be the result of an unresolved import or a typo, which will
            // already get a diagnostic, so don't pile on an extra diagnostic here.
            Type::Dynamic(DynamicType::Unknown) => return,
            _ => {}
        }
        if let Some(builder) = self
            .context
            .report_lint(&INVALID_NEWTYPE, &arguments.args[1])
        {
            let mut diag = builder.into_diagnostic("invalid base for `typing.NewType`");
            diag.set_primary_annotation_message(format!("type `{}`", inferred.display(db, env)));
            if matches!(inferred, Type::ProtocolInstance(_)) {
                diag.info("The base of a `NewType` is not allowed to be a protocol class.");
            } else if matches!(inferred, Type::TypedDict(_)) {
                diag.info("The base of a `NewType` is not allowed to be a `TypedDict`.");
            } else {
                diag.info("The base of a `NewType` must be a class type or another `NewType`.");
            }
        }
    }

    /// Infer a `TypeAliasType("Name", value)` call in a simple assignment context.
    ///
    /// Follows the same pattern as [`Self::infer_newtype_expression`]: validates the
    /// arguments, constructs a [`ManualPEP695TypeAliasType`], and defers inference of
    /// the value argument.
    fn infer_typealiastype_call(
        &mut self,
        target: &ast::Expr,
        call_expr: &ast::ExprCall,
        definition: Definition<'db>,
    ) -> Type<'db> {
        fn error<'db>(
            context: &InferContext<'db, '_>,
            message: impl std::fmt::Display,
            node: impl Ranged,
        ) -> Type<'db> {
            if let Some(builder) = context.report_lint(&INVALID_TYPE_ALIAS_TYPE, node) {
                builder.into_diagnostic(message);
            }
            Type::unknown()
        }

        let db = self.db();
        let arguments = &call_expr.arguments;

        if let Some(starred) = arguments.args.iter().find(|arg| arg.is_starred_expr()) {
            return error(
                &self.context,
                "Starred arguments are not supported in `TypeAliasType` creation",
                starred,
            );
        }

        if arguments.args.len() != 2 {
            return error(
                &self.context,
                format_args!(
                    "Wrong number of arguments in `TypeAliasType` creation: expected 2, found {}",
                    arguments.args.len()
                ),
                call_expr,
            );
        }

        let name_param_ty = self.infer_expression(&arguments.args[0], TypeContext::default());

        let Some(name) = name_param_ty.as_string_literal().map(|name| name.value(db)) else {
            return error(
                &self.context,
                "The first argument to `TypeAliasType` must be a string literal",
                &arguments.args[0],
            );
        };

        let ast::Expr::Name(ast::ExprName {
            id: target_name, ..
        }) = target
        else {
            return error(
                &self.context,
                "A `TypeAliasType` definition must be a simple variable assignment",
                target,
            );
        };

        if name != target_name {
            report_mismatched_type_name(
                &self.context,
                &arguments.args[0],
                "TypeAliasType",
                target_name,
                Some(name),
                name_param_ty,
            );
        }

        // Inference of the value argument must be deferred, to avoid cycles.
        self.deferred.insert(definition);

        Type::KnownInstance(KnownInstanceType::TypeAliasType(
            TypeAliasType::ManualPEP695(ManualPEP695TypeAliasType::new(
                db, name, definition, None, None,
            )),
        ))
    }

    /// Infer the deferred value type of a `TypeAliasType`.
    fn infer_typealiastype_assignment_deferred(
        &mut self,
        definition: Definition<'db>,
        arguments: &ast::Arguments,
    ) {
        let db = self.db();
        // Match the binding context used by eager assignment inference so legacy type variables
        // in the alias value are bound to the alias definition.
        let previous_context = self.typevar_binding_context.replace(definition);

        let value_ty = self.infer_type_expression(&arguments.args[1]);
        let mut type_params = FxHashSet::default();
        let mut valid_type_params = true;
        // Infer keyword arguments (e.g. `type_params`) so their types are stored.
        for keyword in &arguments.keywords {
            self.infer_expression(&keyword.value, TypeContext::default());

            if keyword.arg.as_deref() != Some("type_params") {
                continue;
            }

            let Some(tuple) = keyword.value.as_tuple_expr() else {
                valid_type_params = false;
                if let Some(builder) = self
                    .context
                    .report_lint(&INVALID_TYPE_ALIAS_TYPE, &keyword.value)
                {
                    builder.into_diagnostic(
                        "The `type_params` argument to `TypeAliasType` must be a tuple literal",
                    );
                }
                continue;
            };

            let db = self.db();
            let mut typevar_with_default = None;
            let mut typevar_tuple: Option<TypeVarInstance> = None;
            let mut reported_default_order_error = false;

            for element in &tuple.elts {
                let bound_typevar = match self.expression_type(element) {
                    Type::KnownInstance(KnownInstanceType::TypeVar(typevar)) => bind_typevar(
                        self.db(),
                        self.index,
                        definition.file_scope(db),
                        Some(definition),
                        typevar,
                    ),
                    _ => None,
                };
                let Some(bound_typevar) = bound_typevar else {
                    valid_type_params = false;
                    if let Some(builder) =
                        self.context.report_lint(&INVALID_TYPE_ALIAS_TYPE, element)
                    {
                        builder.into_diagnostic(
                            "Each `type_params` entry for `TypeAliasType` must be a type variable",
                        );
                    }
                    continue;
                };
                let typevar = bound_typevar.typevar(db);

                if bound_typevar.binding_context(db) != BindingContext::Definition(definition) {
                    valid_type_params = false;
                    if let Some(builder) =
                        self.context.report_lint(&INVALID_TYPE_ALIAS_TYPE, element)
                    {
                        builder.into_diagnostic(format_args!(
                            "Type parameter `{}` is bound in an outer scope \
                            and cannot be used in `type_params`",
                            typevar.name(db),
                        ));
                    }
                    continue;
                }

                if !type_params.insert(bound_typevar.identity(db)) {
                    valid_type_params = false;
                    if let Some(builder) =
                        self.context.report_lint(&INVALID_TYPE_ALIAS_TYPE, element)
                    {
                        builder.into_diagnostic(format_args!(
                            "Type parameter `{}` is duplicated in `type_params`",
                            typevar.name(db),
                        ));
                    }
                }

                if typevar
                    .default_type(db, self.program_environment())
                    .is_some()
                {
                    if let Some(typevar_tuple) = typevar_tuple {
                        valid_type_params = false;
                        if let Some(builder) = self
                            .context
                            .report_lint(&INVALID_TYPE_VARIABLE_DEFAULT, element)
                        {
                            builder.into_diagnostic(format_args!(
                                "Type parameter `{}` with a default follows TypeVarTuple `{}`",
                                typevar.name(db),
                                typevar_tuple.name(db),
                            ));
                        }
                    }
                    typevar_with_default.get_or_insert(typevar);
                } else if let Some(typevar_with_default) = typevar_with_default {
                    valid_type_params = false;
                    if !reported_default_order_error
                        && let Some(builder) = self
                            .context
                            .report_lint(&INVALID_TYPE_VARIABLE_DEFAULT, element)
                    {
                        reported_default_order_error = true;
                        builder.into_diagnostic(format_args!(
                            "Type parameter `{}` without a default \
                            cannot follow earlier parameter `{}` with a default",
                            typevar.name(db),
                            typevar_with_default.name(db),
                        ));
                    }
                }

                if typevar.is_typevartuple(db) {
                    if typevar_tuple.is_some() {
                        valid_type_params = false;
                        if let Some(builder) =
                            self.context.report_lint(&INVALID_TYPE_ALIAS_TYPE, element)
                        {
                            builder.into_diagnostic(
                                "Only one `TypeVarTuple` parameter is allowed in `type_params`",
                            );
                        }
                    } else {
                        typevar_tuple = Some(typevar);
                    }
                }
            }
        }

        if valid_type_params {
            let mut value_typevars = FxOrderSet::default();
            value_ty.find_legacy_typevars(
                db,
                self.program_environment(),
                Some(definition),
                &mut value_typevars,
            );

            for typevar in value_typevars {
                if !type_params.contains(&typevar.identity(self.db()))
                    && let Some(builder) = self
                        .context
                        .report_lint(&INVALID_TYPE_ALIAS_TYPE, &arguments.args[1])
                {
                    builder.into_diagnostic(format_args!(
                        "Type parameter `{}` used in the alias value \
                        must be included in `type_params`",
                        typevar.name(self.db()),
                    ));
                }
            }
        }

        self.typevar_binding_context = previous_context;
    }

    fn is_valid_receiver_annotation_target(&self, target: &ast::Expr) -> bool {
        target
            .as_attribute_expr()
            .is_some_and(|target| self.is_receiver_attribute_annotation_target(target))
    }

    fn infer_annotated_assignment_statement(&mut self, assignment: &ast::StmtAnnAssign) {
        // an annotation changes nothing about what the entries mean
        // (`search_fields: list[str] = [...]`), so check the same sites here
        if let Some(value) = &assignment.value {
            self.check_django_field_name_list(&assignment.target, value);
        }
        let db = self.db();
        let env = self.program_environment();
        if let ast::Expr::Name(target) = &*assignment.target {
            // a declaration keyword parses to a synthetic marker in annotation
            // position; an annotation the author actually wrote declares without one
            if !is_declaration_marker(&assignment.annotation) {
                self.report_implicit_declaration(&assignment.target);
            }
            self.report_shadowed_receiver_member(target);
            self.infer_definition(assignment);
        } else {
            // Non-name assignment targets are inferred as ordinary expressions, not definitions.
            let ast::StmtAnnAssign {
                range: _,
                node_index: _,
                annotation,
                value,
                target,
                simple: _,
                decorator_list: _,
                is_context: _,
            } = assignment;
            let annotated = self.infer_annotation_expression(
                annotation,
                DeferredExpressionState::from(self.defer_annotations()),
            );

            if !annotated.qualifiers.is_empty() {
                for qualifier in TypeQualifier::iter() {
                    if !qualifier.is_valid_for_non_name_targets()
                        && annotated
                            .qualifiers
                            .contains(TypeQualifiers::from(qualifier))
                        && let Some(builder) = self
                            .context
                            .report_lint(&INVALID_TYPE_FORM, annotation.as_ref())
                    {
                        builder.into_diagnostic(format_args!(
                            "`{name}` annotations are not allowed for non-name targets",
                            name = qualifier.name()
                        ));
                    }
                }
            }

            // P.args and P.kwargs are only valid as annotations on *args and **kwargs.
            // basedpython has no source spelling for them at all, and says so once where the
            // type expression is resolved
            if !self.is_basedpython_file()
                && let Type::TypeVar(typevar) = annotated.inner_type()
                && typevar.is_paramspec(self.db())
                && let Some(attr) = typevar.paramspec_attr(self.db())
            {
                let name = typevar.name(self.db());
                let (attr_name, variadic) = match attr {
                    ParamSpecAttrKind::Args => ("args", "*args"),
                    ParamSpecAttrKind::Kwargs => ("kwargs", "**kwargs"),
                };
                if let Some(builder) = self
                    .context
                    .report_lint(&INVALID_PARAMSPEC, annotation.as_ref())
                {
                    builder.into_diagnostic(format_args!(
                        "`{name}.{attr_name}` is only valid \
                        for annotating `{variadic}` function parameters",
                    ));
                }
            } else if let ast::Expr::Attribute(attr_expr) = annotation.as_ref()
                && matches!(attr_expr.attr.as_str(), "args" | "kwargs")
            {
                let value_ty = self.expression_type(&attr_expr.value);
                if let Type::KnownInstance(KnownInstanceType::TypeVar(typevar)) = value_ty
                    && typevar.is_paramspec(self.db())
                {
                    let name = typevar.name(self.db());
                    let attr_name = &attr_expr.attr;
                    let variadic = if attr_name == "args" {
                        "*args"
                    } else {
                        "**kwargs"
                    };
                    if let Some(builder) = self
                        .context
                        .report_lint(&INVALID_PARAMSPEC, annotation.as_ref())
                    {
                        builder.into_diagnostic(format_args!(
                            "`{name}.{attr_name}` is only valid \
                            for annotating `{variadic}` function parameters",
                        ));
                    }
                }
            }

            // Disallow annotations on non-name targets unless they are valid receivers (e.g.
            // `self.x: int` or `cls.x: int`).
            if !self.is_valid_receiver_annotation_target(target) {
                let message = match target.as_ref() {
                    ast::Expr::Attribute(_) => {
                        "Type annotations are not allowed on this attribute expression"
                    }
                    ast::Expr::Subscript(_) => {
                        "Type annotations are not allowed on subscripted expressions"
                    }
                    _ => {
                        // For parser-recovered invalid targets, the syntax diagnostic is
                        // sufficient.
                        if let Some(value) = value {
                            self.infer_maybe_standalone_expression(value, TypeContext::default());
                        }
                        self.infer_expression(target, TypeContext::default());
                        return;
                    }
                };

                // For syntactically valid non-name targets, reject the annotation and validate
                // any accompanying assignment.
                if let Some(builder) = self
                    .context
                    .report_lint(&INVALID_TYPE_FORM, annotation.as_ref())
                {
                    builder.into_diagnostic(message);
                }

                if let Some(value) = value {
                    self.infer_target(target, value, &|builder, tcx| {
                        builder.infer_maybe_standalone_expression(value, tcx)
                    });
                } else {
                    self.infer_expression(target, TypeContext::default());
                }
                return;
            }

            let value_ty = value.as_ref().map(|value| {
                self.infer_maybe_standalone_expression(
                    value,
                    TypeContext::new(Some(annotated.inner_type())),
                )
            });

            // If we have an annotated assignment like `self.attr: int = 1`, we still need to
            // do type inference on the `self.attr` target to get types for all sub-expressions.
            self.infer_expression(target, TypeContext::default());
            if let ast::Expr::Attribute(target) = target.as_ref() {
                self.report_undeclared_protocol_attribute(target);
            }

            // For annotated assignments like `self.x: Final[int] = 1`, the `Final` qualifier
            // comes from the annotation itself, so we can check it directly rather than
            // looking up qualifiers from the object type (as `validate_final_attribute_assignment`
            // does for augmented assignments).
            if value.is_some()
                && annotated.qualifiers.contains(TypeQualifiers::FINAL)
                && let ast::Expr::Attribute(attr_expr) = target.as_ref()
            {
                let object_ty = self.expression_type(&attr_expr.value);
                self.invalid_assignment_to_final_attribute(
                    object_ty,
                    attr_expr,
                    attr_expr.attr.id(),
                    annotated.qualifiers,
                );
            }

            // But here we explicitly overwrite the type for the overall `self.attr` node.
            // We do not use `store_expression_type` here, because it checks that no type
            // has been stored for the expression before. When there's a value, use the
            // inferred type (matching the name-target definition path); otherwise fall
            // back to the annotated type. If the value is not assignable to the declared
            // type, report an error and fall back to the annotated type.
            let target_ty = if let Some(value_ty) = value_ty {
                let declared_ty = annotated.inner_type();
                if value_ty.is_assignable_to(db, env, declared_ty) {
                    value_ty
                } else {
                    if let Some(builder) = self
                        .context
                        .report_lint(&INVALID_ASSIGNMENT, value.as_deref().unwrap())
                    {
                        let mut diag = builder.into_diagnostic(format_args!(
                            "Object of type `{}` is not assignable to `{}`",
                            value_ty.display(db, env),
                            declared_ty.display(db, env),
                        ));
                        diag.annotate(
                            self.context
                                .secondary(annotation.as_ref())
                                .message("Declared type"),
                        );
                        diag.set_primary_annotation_message(format_args!(
                            "Incompatible value of type `{}`",
                            value_ty.display(db, env),
                        ));
                    }
                    declared_ty
                }
            } else {
                annotated.inner_type()
            };
            self.expressions.insert((&**target).into(), target_ty);
        }
    }

    /// Infer an annotated assignment's annotation using the file's deferred-annotation semantics.
    fn infer_annotated_assignment_annotation(
        &mut self,
        assignment: &AnnotatedAssignmentDefinitionKind,
    ) -> TypeAndQualifiers<'db> {
        let annotation = assignment.annotation(self.module());

        // PEP 681 lets a field specifier appear in the annotation's `Annotated`
        // metadata (`x: Annotated[int, Field(default=0)]`), so recognize
        // field-specifier calls while inferring the annotation, exactly as an
        // r.h.s. value does. Only populated inside a dataclass-like class body;
        // cleared immediately after so it does not leak into the value.
        self.setup_dataclass_field_specifiers();
        let declared = self.infer_annotation_expression_allow_pep_613(
            annotation,
            DeferredExpressionState::from(self.defer_annotations()),
        );
        self.dataclass_field_specifiers.clear();

        declared
    }

    /// Initialize a declaration cycle without discarding its annotation diagnostics or metadata.
    pub(super) fn infer_annotated_assignment_cycle_initial(
        mut self,
        definition: Definition<'db>,
        assignment: &AnnotatedAssignmentDefinitionKind,
        cycle_recovery: Type<'db>,
    ) -> DefinitionInference<'db> {
        let declared = self.infer_annotated_assignment_annotation(assignment);
        self.declarations.insert(definition, declared);
        self.cycle_recovery = Some(cycle_recovery);
        self.finish_inferred_definition(definition)
    }

    /// Infer the types in an annotated assignment definition.
    fn infer_annotated_assignment_definition(
        &mut self,
        assignment: &'db AnnotatedAssignmentDefinitionKind,
        definition: Definition<'db>,
    ) {
        let db = self.db();
        let env = self.program_environment();
        let target = assignment.target(self.module());
        let value = assignment.value(self.module());

        if !target.is_name_expr() && !self.is_valid_receiver_annotation_target(target) {
            // Omit this definition from `self.declarations`; declaration lookup treats an absent
            // inferred declaration as rejected.
            if !definition
                .kind(self.db())
                .category(self.in_stub(), self.module())
                .is_binding()
            {
                return;
            }

            let node = target.into();
            let add = AddBinding {
                declared_ty: self.fallback_member_declared_type(node),
                binding: definition,
                node,
                qualifiers: TypeQualifiers::empty(),
                is_local: true,
            };
            let target_ty = if let Some(value) = value {
                // Infer the value as an ordinary assignment without using the rejected annotation
                // as its declared type.
                let value_ty = self.infer_maybe_standalone_expression(value, add.type_context());
                self.stub_placeholder_binding_type(value)
                    .unwrap_or(value_ty)
            } else {
                // Annotation-only definitions are bindings in stubs.
                add.declared_ty.unwrap_or(Type::unknown())
            };
            self.store_expression_type(target, target_ty);
            add.insert(self, target_ty);

            return;
        }

        let annotation = assignment.annotation(self.module());

        // basedpython `newtype Foo = int` parses as AnnAssign with annotation
        // `__newtype__` and the underlying type stored as the value. produce a
        // `KnownInstanceType::NewType` directly so `Foo` behaves like a NewType
        // without going through the regular `Foo = NewType("Foo", int)` path
        if let ast::Expr::Name(ann_name) = annotation
            && ann_name.id.as_str() == "__newtype__"
            && let ast::Expr::Name(target_name) = target
            && let Some(value_expr) = value
        {
            let base_ty = self.infer_type_expression(value_expr);
            let eager_base = match base_ty {
                Type::NominalInstance(nominal) => Some(
                    crate::types::newtype::NewTypeBase::ClassType(nominal.class(self.db(), env)),
                ),
                Type::NewTypeInstance(nt) => Some(crate::types::newtype::NewTypeBase::NewType(nt)),
                Type::Union(union) => match union.known(self.db()) {
                    Some(crate::types::KnownUnion::Float) => {
                        Some(crate::types::newtype::NewTypeBase::Float)
                    }
                    Some(crate::types::KnownUnion::Complex) => {
                        Some(crate::types::newtype::NewTypeBase::Complex)
                    }
                    _ => None,
                },
                _ => None,
            };
            let newtype = crate::types::newtype::NewType::new(
                self.db(),
                target_name.id.clone(),
                definition,
                eager_base,
            );
            let inferred_ty = Type::KnownInstance(KnownInstanceType::NewType(newtype));
            self.store_expression_type(annotation, Type::unknown());
            self.store_qualifiers(annotation, TypeQualifiers::empty());
            self.store_expression_type(target, inferred_ty);
            self.add_declaration_with_binding(
                target.into(),
                definition,
                &DeclaredAndInferredType::are_the_same_type(inferred_ty),
            );
            return;
        }

        // basedpython: a bare `final` modifier on an assignment (`final a = 1`)
        // lowers to a plain assignment and leaves the variable no more final
        // than before — the user almost certainly meant `let`. `final override`
        // is a real marker, so only fire when `override` is absent. inside a
        // class body a bare `final` assignment is a plain attribute, matching
        // `let`-in-class, so restrict this to non-class scopes.
        if let ast::Expr::Name(ann_name) = annotation
            && ann_name.id.as_str() == "__modifier_assign__"
            && let ast::Expr::Name(target_name) = target
            && self
                .index
                .scope(self.scope().file_scope_id(self.db()))
                .kind()
                != ScopeKind::Class
        {
            let source = source_text(self.db(), self.file());
            let modifiers = &source[ann_name.range()];
            let has_final = modifiers.split_whitespace().any(|kw| kw == "final");
            let has_override = modifiers.split_whitespace().any(|kw| kw == "override");
            if has_final
                && !has_override
                && let Some(builder) = self.context.report_lint(&FINAL_ON_VARIABLE, ann_name)
            {
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "`final` on variable `{name}` has no effect; use `let` instead",
                    name = target_name.id
                ));
                diagnostic.info("a final variable is declared with `let`, which lowers to `Final`");
            }
        }

        // basedpython: a declaration keyword on an unannotated assignment
        // (`var a = 1`, `override a = 1`) states no type, so it binds without
        // declaring — the definition is categorized as a binding to match. going
        // through the declaration path instead would record a declared `Unknown`
        // and hide the inferred type from every reader that prefers a
        // declaration (an importer of the module, a class attribute lookup)
        if is_untyped_declaration_marker(annotation)
            && let Some(value) = value
        {
            self.store_expression_type(annotation, Type::unknown());
            self.store_qualifiers(annotation, TypeQualifiers::empty());
            let add = self.add_binding(target.into(), definition);
            let value_ty = self.infer_maybe_standalone_expression(value, add.type_context());
            let target_ty = self
                .stub_placeholder_binding_type(value)
                .unwrap_or(value_ty);
            let target_ty =
                self.infer_binding_decorators(assignment.decorators(self.module()), target_ty);
            self.store_expression_type(target, target_ty);
            add.insert(self, target_ty);
            return;
        }

        // basedpython: an unannotated `field = <init>` in a property accessor block
        // carries `__field__[T]`, where `T` is the property's declared type. The
        // field has no declared type of its own — `T` is only the context the
        // initialiser is solved against, so a bare `[]` under a `Sequence[int]`
        // property declares `list[int]` rather than `list[Unknown]` — and the
        // inferred type becomes the declaration, keeping storage typed
        // independently of the property
        if let Some(context) = untyped_declaration_context(annotation)
            && let Some(value) = value
        {
            let context_ty = self.infer_type_expression(context);
            self.store_expression_type(annotation, Type::unknown());
            self.store_qualifiers(annotation, TypeQualifiers::empty());
            let add = self.add_binding(target.into(), definition);
            let value_ty =
                self.infer_maybe_standalone_expression(value, TypeContext::new(Some(context_ty)));
            self.store_expression_type(target, value_ty);
            add.insert(self, value_ty);
            return;
        }

        let mut declared = self.infer_annotated_assignment_annotation(assignment);

        // basedpython: `let x: T` declares read-only state in every scope, with or
        // without an initializer. the `__let__` marker only marks `FINAL` outside
        // class scope, so add it here for the in-class case (idempotent at module
        // scope, already `FINAL`).
        //
        // `FINAL` is what enforces "no reassignment away from the declaration"; it
        // does *not* close the attribute to subclasses, because `is_let_declaration`
        // exempts a `let` from the override-of-final check. no `Final` is emitted in
        // the lowered python either — read-only-ness is a type-checker-only marker
        let is_let_marker = match annotation {
            ast::Expr::Name(n) => n.id.as_str() == "__let__",
            ast::Expr::Subscript(s) => {
                matches!(s.value.as_ref(), ast::Expr::Name(n) if n.id.as_str() == "__let__")
            }
            _ => false,
        };
        if is_let_marker {
            declared = declared.with_qualifier(TypeQualifiers::FINAL);
        }

        // P.args and P.kwargs are only valid as annotations on *args and **kwargs,
        // not as variable annotations. Check both resolved type and AST form. basedpython has
        // no source spelling for them at all, and says so once where the type expression is
        // resolved
        if !self.is_basedpython_file()
            && let Type::TypeVar(typevar) = declared.inner_type()
            && typevar.is_paramspec(self.db())
            && let Some(attr) = typevar.paramspec_attr(self.db())
        {
            let name = typevar.name(self.db());
            let (attr_name, variadic) = match attr {
                ParamSpecAttrKind::Args => ("args", "*args"),
                ParamSpecAttrKind::Kwargs => ("kwargs", "**kwargs"),
            };
            if let Some(builder) = self.context.report_lint(&INVALID_PARAMSPEC, annotation) {
                builder.into_diagnostic(format_args!(
                    "`{name}.{attr_name}` is only valid \
                    for annotating `{variadic}` function parameters",
                ));
            }
        } else if let ast::Expr::Attribute(attr_expr) = annotation
            && matches!(attr_expr.attr.as_str(), "args" | "kwargs")
        {
            // Also check the AST form for cases where P isn't bound (e.g., class body
            // annotations). In this case, the type might not resolve to a TypeVar.
            let value_ty = self.expression_type(&attr_expr.value);
            if let Type::KnownInstance(KnownInstanceType::TypeVar(typevar)) = value_ty
                && typevar.is_paramspec(self.db())
            {
                let name = typevar.name(self.db());
                let attr_name = &attr_expr.attr;
                let variadic = if attr_name == "args" {
                    "*args"
                } else {
                    "**kwargs"
                };
                if let Some(builder) = self.context.report_lint(&INVALID_PARAMSPEC, annotation) {
                    builder.into_diagnostic(format_args!(
                        "`{name}.{attr_name}` is only valid \
                        for annotating `{variadic}` function parameters",
                    ));
                }
            }
        }

        let is_pep_613_type_alias = declared.inner_type().is_typealias_special_form();

        if !declared.qualifiers.is_empty() {
            for qualifier in TypeQualifier::iter() {
                if !declared
                    .qualifiers
                    .contains(TypeQualifiers::from(qualifier))
                {
                    continue;
                }
                let current_scope_id = self.scope().file_scope_id(self.db());

                if self.index.scope(current_scope_id).kind() != ScopeKind::Class {
                    match qualifier {
                        TypeQualifier::Final => {}
                        TypeQualifier::ClassVar => {
                            if let Some(builder) =
                                self.context.report_lint(&INVALID_TYPE_FORM, annotation)
                            {
                                builder
                                    .into_diagnostic("`ClassVar` is only allowed in class bodies");
                            }
                        }
                        TypeQualifier::InitVar => {
                            if let Some(builder) =
                                self.context.report_lint(&INVALID_TYPE_FORM, annotation)
                            {
                                builder.into_diagnostic(
                                    "`InitVar` is only allowed in dataclass fields",
                                );
                            }
                        }
                        TypeQualifier::NotRequired
                        | TypeQualifier::ReadOnly
                        | TypeQualifier::Required => {
                            if let Some(builder) =
                                self.context.report_lint(&INVALID_TYPE_FORM, annotation)
                            {
                                builder.into_diagnostic(format_args!(
                                    "`{name}` is only allowed in TypedDict fields",
                                    name = qualifier.name()
                                ));
                            }
                        }
                    }

                    continue;
                }

                let nearest_enclosing_class = nearest_enclosing_class(db, self.index, self.scope());
                let class_kind = nearest_enclosing_class.and_then(|class| {
                    CodeGeneratorKind::from_class(self.db(), ClassLiteral::Static(class))
                });

                match class_kind {
                    Some(CodeGeneratorKind::TypedDict) => {
                        if !qualifier.is_valid_in_typeddict_field()
                            && let Some(builder) =
                                self.context.report_lint(&INVALID_TYPE_FORM, annotation)
                        {
                            builder.into_diagnostic(format_args!(
                                "`{name}` is not allowed in TypedDict fields",
                                name = qualifier.name()
                            ));
                        }
                    }
                    Some(
                        class_kind @ (CodeGeneratorKind::DataclassLike(_)
                        | CodeGeneratorKind::Pydantic(_)),
                    ) => match qualifier {
                        TypeQualifier::NotRequired
                        | TypeQualifier::ReadOnly
                        | TypeQualifier::Required => {
                            let Some(builder) =
                                self.context.report_lint(&INVALID_TYPE_FORM, annotation)
                            else {
                                continue;
                            };
                            let field_kind = class_kind.name();
                            builder.into_diagnostic(format_args!(
                                "`{name}` is not allowed in {field_kind} fields",
                                name = qualifier.name(),
                            ));
                        }
                        TypeQualifier::ClassVar | TypeQualifier::Final | TypeQualifier::InitVar => {
                        }
                    },
                    Some(
                        CodeGeneratorKind::NamedTuple
                        | CodeGeneratorKind::Django
                        | CodeGeneratorKind::SqlalchemyDeclarative,
                    )
                    | None => match qualifier {
                        TypeQualifier::NotRequired
                        | TypeQualifier::Required
                        | TypeQualifier::ReadOnly => {
                            let Some(builder) =
                                self.context.report_lint(&INVALID_TYPE_FORM, annotation)
                            else {
                                continue;
                            };
                            builder.into_diagnostic(format_args!(
                                "`{name}` is only allowed in TypedDict fields",
                                name = qualifier.name()
                            ));
                        }
                        TypeQualifier::InitVar => {
                            let Some(builder) =
                                self.context.report_lint(&INVALID_TYPE_FORM, annotation)
                            else {
                                continue;
                            };
                            builder
                                .into_diagnostic("`InitVar` is only allowed in dataclass fields");
                        }
                        TypeQualifier::ClassVar | TypeQualifier::Final => {}
                    },
                }
            }
        }

        if target
            .as_name_expr()
            .is_some_and(|name| &name.id == "TYPE_CHECKING")
        {
            if !KnownClass::Bool.to_instance(db, env).is_assignable_to(
                db,
                env,
                declared.inner_type(),
            ) {
                // annotation not assignable from `bool` is an error
                report_invalid_type_checking_constant(&self.context, target.into());
            } else if self.in_stub()
                && value
                    .as_ref()
                    .is_none_or(|value| value.is_ellipsis_literal_expr())
            {
                // stub file assigning nothing or `...` is fine
            } else if !matches!(
                value
                    .as_ref()
                    .and_then(|value| value.as_boolean_literal_expr()),
                Some(ast::ExprBooleanLiteral { value: false, .. })
            ) {
                // otherwise, assigning something other than `False` is an error
                report_invalid_type_checking_constant(&self.context, target.into());
            }
            declared.inner = Type::bool_literal(true);
        }

        // Handle various singletons.
        if let Some(name_expr) = target.as_name_expr()
            && let Some(special_form) = SpecialFormType::try_from_file_and_name(
                self.db(),
                ImportingFile::File(
                    self.file(),
                    self.program_environment().resolver_environment(self.db()),
                ),
                &name_expr.id,
            )
        {
            declared.inner = Type::SpecialForm(special_form);
        }

        // If the target of an assignment is not one of the place expressions we support,
        // then they are not definitions, so we can only be here if the target is in a form supported as a place expression.
        // In this case, we can simply store types in `target` below, instead of calling `infer_expression` (which would return `Never`).
        debug_assert!(PlaceExpr::try_from_expr(target).is_some());

        if let Some(value) = value {
            self.setup_dataclass_field_specifiers();

            // We defer the r.h.s. of PEP-613 `TypeAlias` assignments in stub files.
            let previous_deferred_state = self.deferred_state;

            if is_pep_613_type_alias {
                self.context.inference_flags |= InferenceFlags::IN_PEP_613_ALIAS_FIRST_PASS;
                if self.in_stub() {
                    self.deferred_state = DeferredExpressionState::Deferred;
                }
            }

            // This might be a PEP-613 type alias (`OptionalList: TypeAlias = list[T] | None`). Use
            // the definition of `OptionalList` as the binding context while inferring the
            // RHS (`list[T] | None`), in order to bind `T` to `OptionalList`.
            let previous_typevar_binding_context = self.typevar_binding_context.replace(definition);

            let inferred_ty = self.infer_maybe_standalone_expression(
                value,
                TypeContext::new(Some(declared.inner_type())),
            );
            let inferred_ty = if is_pep_613_type_alias && target.is_name_expr() {
                // The post-inference pass emits the diagnostic, but this first-pass value is
                // retained as the alias binding.
                match inferred_ty {
                    Type::SpecialForm(SpecialFormType::TypingSelf) => {
                        self.expressions.insert(value.into(), Type::unknown());
                        Type::unknown()
                    }
                    Type::KnownInstance(KnownInstanceType::LiteralStringAlias(ty))
                        if ty.inner(self.db()).contains_self(db, env) =>
                    {
                        Type::KnownInstance(KnownInstanceType::LiteralStringAlias(
                            InternedType::new(self.db(), Type::unknown()),
                        ))
                    }
                    _ => inferred_ty,
                }
            } else {
                inferred_ty
            };

            self.typevar_binding_context = previous_typevar_binding_context;
            self.deferred_state = previous_deferred_state;
            self.dataclass_field_specifiers.clear();
            self.context
                .inference_flags
                .remove(InferenceFlags::IN_PEP_613_ALIAS_FIRST_PASS);

            let inferred_ty = if target
                .as_name_expr()
                .is_some_and(|name| &name.id == "TYPE_CHECKING")
            {
                Type::bool_literal(true)
            } else if self.in_stub() && value.is_ellipsis_literal_expr() {
                declared.inner_type()
            } else {
                inferred_ty
            };

            // basedpython: a decorator written above the declaration applies to what
            // it binds, so the declared type is checked against what the decorator
            // returns rather than against the value written under it
            let inferred_ty =
                self.infer_binding_decorators(assignment.decorators(self.module()), inferred_ty);

            if is_pep_613_type_alias {
                let inferred_ty =
                    if let Type::KnownInstance(KnownInstanceType::TypeVar(typevar)) = inferred_ty {
                        let identity = TypeVarIdentity::new(
                            self.db(),
                            typevar.identity(self.db()).name(self.db()),
                            typevar.identity(self.db()).definition(self.db()),
                            TypeVarKind::Pep613Alias,
                        );
                        Type::KnownInstance(KnownInstanceType::TypeVar(
                            typevar.with_identity(self.db(), identity),
                        ))
                    } else {
                        inferred_ty
                    };
                self.add_declaration_with_binding(
                    target.into(),
                    definition,
                    &DeclaredAndInferredType::AreTheSame(TypeAndQualifiers::declared(inferred_ty)),
                );
            } else {
                // Check for annotated enum members. The typing spec states that enum
                // members should not have explicit type annotations.
                if let Some(name_expr) = target.as_name_expr()
                    && !name_expr.id.starts_with("__")
                    && !matches!(name_expr.id.as_str(), "_ignore_" | "_value_" | "_name_")
                    && (
                        // Not bare Final (bare Final is allowed on enum members)
                        !(declared.qualifiers.contains(TypeQualifiers::FINAL)
                            && matches!(declared.inner_type(), Type::Dynamic(DynamicType::Unknown)))
                    )
                    && (
                        // Value type would be an enum member at runtime (exclude callables,
                        // which are never members)
                        !inferred_ty.is_subtype_of(
                            db,
                            env,
                            Type::Callable(CallableType::unknown(self.db()))
                                .top_materialization(db, env),
                        )
                    )
                {
                    let current_scope_id = self.scope().file_scope_id(self.db());
                    let current_scope = self.index.scope(current_scope_id);
                    if current_scope.kind() == ScopeKind::Class
                        && let Some(class) = nearest_enclosing_class(db, self.index, self.scope())
                        && is_enum_class_by_inheritance(db, env, class)
                        && !enum_ignored_names(self.db(), self.scope()).contains(&name_expr.id)
                        && let Some(builder) = self
                            .context
                            .report_lint(&INVALID_ENUM_MEMBER_ANNOTATION, annotation)
                    {
                        let mut diag = builder.into_diagnostic(format_args!(
                            "Type annotation on enum member `{}` is not allowed",
                            name_expr.id
                        ));
                        diag.info(
                            "See: https://typing.python.org/en/latest/spec/enums.html#enum-members",
                        );
                    }
                }

                self.add_declaration_with_binding(
                    target.into(),
                    definition,
                    &DeclaredAndInferredType::MightBeDifferent {
                        declared_ty: declared,
                        inferred_ty,
                    },
                );
            }

            self.store_expression_type(target, inferred_ty);
        } else {
            if is_pep_613_type_alias {
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, annotation) {
                    builder.into_diagnostic(
                        "`TypeAlias` must be assigned a value in annotated assignments",
                    );
                }
                declared.inner = Type::unknown();
            }
            if self.in_stub() {
                self.add_declaration_with_binding(
                    target.into(),
                    definition,
                    &DeclaredAndInferredType::AreTheSame(declared),
                );
            } else {
                self.add_declaration(target.into(), definition, declared);
            }

            self.store_expression_type(target, declared.inner_type());
        }
    }

    fn infer_augmented_assignment_statement(&mut self, assignment: &ast::StmtAugAssign) {
        if assignment.target.is_name_expr() {
            self.infer_definition(assignment);
        } else {
            // Non-name assignment targets are inferred as ordinary expressions, not definitions.
            if let Ok(result_ty) = self.infer_augment_assignment(assignment) {
                let target = assignment.target.as_ref();
                match target {
                    ast::Expr::Attribute(attribute) => {
                        let object_ty = self.expression_type(&attribute.value);
                        self.validate_attribute_assignment(
                            attribute,
                            target,
                            object_ty,
                            attribute.attr.id(),
                            &mut |_, _| result_ty,
                            true,
                        );
                    }
                    ast::Expr::Subscript(subscript) => {
                        let object_ty = self.expression_type(&subscript.value);
                        let slice_ty = self.expression_type(&subscript.slice);
                        self.validate_subscript_assignment(
                            subscript,
                            target,
                            object_ty,
                            &mut |_, _| slice_ty,
                            &mut |_, _| result_ty,
                        );
                    }
                    _ => {}
                }
            }

            if let ast::Expr::Attribute(attr_expr) = assignment.target.as_ref() {
                self.report_undeclared_protocol_attribute(attr_expr);
            }
        }
    }

    /// Infer an augmented operator, returning its recovery type if the operation fails.
    fn infer_augmented_op(
        &mut self,
        assignment: &ast::StmtAugAssign,
        target_type: Type<'db>,
        value_expr: &ast::Expr,
        infer_value_ty: &mut dyn FnMut(&mut Self, TypeContext<'db>) -> Type<'db>,
    ) -> Result<Type<'db>, Type<'db>> {
        let db = self.db();
        let env = self.program_environment();
        // If the target defines, e.g., `__iadd__`, infer the augmented assignment as a call to that
        // dunder.
        let op = assignment.op;

        // Fall back to non-augmented binary operator inference.
        let binary_return_ty = |builder: &mut Self, value_ty| {
            builder
                .infer_binary_expression_type(
                    assignment.into(),
                    false,
                    target_type,
                    value_ty,
                    op,
                    TypeContext::default(),
                )
                // an extension's operator dunder is deliberately *not* consulted
                // here: an augmented assignment has no lowering to the backing
                // function (rewriting `a += b` re-evaluates the target), so
                // accepting it would put the checker and the runtime at odds
                .ok_or_else(|| {
                    report_unsupported_augmented_assignment(
                        &builder.context,
                        assignment,
                        target_type,
                        value_ty,
                    );
                    Type::unknown()
                })
        };

        match target_type {
            Type::Union(union) => {
                let mut infer_value_ty = MultiInferenceGuard::new(infer_value_ty);

                // Perform loud inference without type context, as there may be multiple
                // equally applicable type contexts for each union member.
                infer_value_ty.infer_loud(self, TypeContext::default());

                let mut operation_failed = false;
                let result_ty = union.map(db, env, |&elem_type| {
                    match self.infer_augmented_op(
                        assignment,
                        elem_type,
                        value_expr,
                        &mut |builder, tcx| infer_value_ty.infer_silent(builder, tcx),
                    ) {
                        Ok(ty) => ty,
                        Err(recovery_ty) => {
                            operation_failed = true;
                            recovery_ty
                        }
                    }
                });

                if operation_failed {
                    Err(result_ty)
                } else {
                    Ok(result_ty)
                }
            }

            _ => {
                if let Some(typed_dict_update_ty) = self
                    .try_infer_typed_dict_pep_584_augmented_assignment(
                        assignment,
                        target_type,
                        value_expr,
                        infer_value_ty,
                    )
                {
                    return Ok(typed_dict_update_ty);
                }

                let ast_arguments = [ArgOrKeyword::Arg(value_expr)];
                let mut call_arguments = CallArguments::positional([Type::unknown()]);

                let call = self.infer_and_try_call_dunder(
                    target_type,
                    op.in_place_dunder(),
                    MemberLookupPolicy::NO_INSTANCE_FALLBACK,
                    ArgumentsIter::synthesized(&ast_arguments),
                    &mut call_arguments,
                    &mut |builder, (_, _, tcx)| infer_value_ty(builder, tcx),
                    TypeContext::default(),
                );
                match call {
                    Ok(outcome) => Ok(outcome.return_type(db, env)),
                    Err(CallDunderError::MethodNotAvailable) => {
                        let value_ty = infer_value_ty(self, TypeContext::default());
                        binary_return_ty(self, value_ty)
                    }
                    Err(CallDunderError::PossiblyUnbound {
                        bindings: outcome, ..
                    }) => {
                        let value_ty = outcome.type_for_argument(&call_arguments, 0);
                        match binary_return_ty(self, value_ty) {
                            Ok(binary_ty) => Ok(UnionType::from_two_elements(
                                db,
                                env,
                                outcome.return_type(db, env),
                                binary_ty,
                            )),
                            Err(recovery_ty) => Err(UnionType::from_two_elements(
                                db,
                                env,
                                outcome.return_type(db, env),
                                recovery_ty,
                            )),
                        }
                    }
                    Err(CallDunderError::CallError(_, bindings, _)) => {
                        let value_ty = bindings.type_for_argument(&call_arguments, 0);
                        report_unsupported_augmented_assignment(
                            &self.context,
                            assignment,
                            target_type,
                            value_ty,
                        );
                        Err(bindings.return_type(db, env))
                    }
                }
            }
        }
    }

    fn infer_augment_assignment_definition(
        &mut self,
        assignment: &'ast ast::StmtAugAssign,
        definition: Definition<'db>,
    ) {
        let target_ty = self
            .infer_augment_assignment(assignment)
            .unwrap_or_else(|recovery_ty| recovery_ty);
        self.add_binding(assignment.target.as_ref().into(), definition)
            .insert(self, target_ty);
    }

    fn infer_augment_assignment(
        &mut self,
        assignment: &ast::StmtAugAssign,
    ) -> Result<Type<'db>, Type<'db>> {
        let ast::StmtAugAssign {
            range: _,
            node_index: _,
            target,
            op: _,
            value,
        } = assignment;

        // Resolve the target type, assuming a load context.
        let target_result = match &**target {
            ast::Expr::Name(name) => {
                let previous_value = self.infer_name_load(name, TypeContext::default());
                self.store_expression_type(target, previous_value);
                Ok(previous_value)
            }
            ast::Expr::Attribute(attr) => {
                let result = self.infer_attribute_load(attr);
                let previous_value = result.unwrap_or_else(|recovery_ty| recovery_ty);
                self.store_expression_type(target, previous_value);
                result
            }
            ast::Expr::Subscript(subscript) => {
                let result = self.infer_subscript_load(subscript, TypeContext::default());
                let previous_value = result.unwrap_or_else(|recovery_ty| recovery_ty);
                self.store_expression_type(target, previous_value);
                result
            }
            _ => Ok(self.infer_expression(target, TypeContext::default())),
        };

        let target_type = target_result.unwrap_or_else(|recovery_ty| recovery_ty);
        let operation_result =
            self.infer_augmented_op(assignment, target_type, value, &mut |builder, tcx| {
                builder.infer_expression(value, tcx)
            });

        match (target_result, operation_result) {
            (Ok(_), Ok(result_ty)) => Ok(result_ty),
            (_, Ok(recovery_ty) | Err(recovery_ty)) => Err(recovery_ty),
        }
    }

    fn infer_dict_key_assignment_definition(
        &mut self,
        key: &'ast ast::Expr,
        value: &'ast ast::Expr,
        assignment: Definition<'db>,
        definition: Definition<'db>,
    ) {
        let value_ty = infer_definition_types(self.db(), assignment).expression_type(value);
        self.add_binding(key.into(), definition)
            .insert(self, value_ty);
    }

    fn infer_type_alias_statement(&mut self, node: &ast::StmtTypeAlias) {
        self.infer_definition(node);
    }

    fn fixed_length_iterable_element_type(
        &self,
        iterable: &ast::Expr,
        expression_type: impl FnMut(&ast::Expr) -> Type<'db>,
    ) -> Option<Type<'db>> {
        let db = self.db();
        let env = self.program_environment();
        let element_types =
            extract_fixed_length_iterable_element_types(db, env, iterable, expression_type)?;

        if element_types.is_empty() {
            None
        } else {
            Some(UnionType::from_elements(
                db,
                env,
                element_types.iter().copied(),
            ))
        }
    }

    fn infer_for_statement(&mut self, for_statement: &ast::StmtFor) {
        let db = self.db();
        let ast::StmtFor {
            range: _,
            node_index: _,
            target,
            pattern,
            iter,
            body,
            orelse,
            is_async,
        } = for_statement;

        self.infer_target(target, iter, &|builder, tcx| {
            // TODO: `infer_for_statement_definition` reports a diagnostic if `iter_ty` isn't iterable
            //  but only if the target is a name. We should report a diagnostic here if the target isn't a name:
            //  `for a.x in not_iterable: ...
            let iterable_type = builder.infer_standalone_expression(iter, tcx);
            if !*is_async
                && let Some(element_type) = builder
                    .fixed_length_iterable_element_type(iter, |expr| builder.expression_type(expr))
            {
                element_type
            } else {
                let env = builder.program_environment();
                iterable_type
                    .iterate(db, env)
                    .homogeneous_element_type(db, env)
            }
        });

        // basedpython: the loop destructures each element
        if let Some(pattern) = pattern.as_deref() {
            self.infer_match_pattern(pattern);
            self.check_destructure(pattern);
        }

        self.infer_body(body);
        self.infer_body(orelse);
    }

    fn infer_for_statement_definition(
        &mut self,
        for_stmt: &ForStmtDefinitionKind<'db>,
        definition: Definition<'db>,
    ) {
        let db = self.db();
        let iterable = for_stmt.iterable(self.module());
        let target = for_stmt.target(self.module());

        let loop_var_value_type = match for_stmt.target_kind() {
            TargetKind::Sequence(unpack_position, unpack) => {
                let unpacked = infer_unpack_types(self.db(), unpack);
                if unpack_position == UnpackPosition::First {
                    self.context.extend(unpacked.diagnostics());
                }

                unpacked.expression_type(target)
            }
            TargetKind::Single => {
                let iterable_type =
                    self.infer_standalone_expression(iterable, TypeContext::default());

                report_iteration_over_character(&self.context, iterable_type, iterable.into());

                if !for_stmt.is_async()
                    && let Some(element_type) = self
                        .fixed_length_iterable_element_type(iterable, |expr| {
                            self.expression_type(expr)
                        })
                {
                    element_type
                } else {
                    let env = self.program_environment();
                    iterable_type
                        .try_iterate_with_mode(
                            db,
                            env,
                            EvaluationMode::from_is_async(for_stmt.is_async()),
                        )
                        .map(|tuple| tuple.homogeneous_element_type(db, env))
                        .unwrap_or_else(|err| {
                            err.report_diagnostic(&self.context, iterable_type, iterable.into());
                            err.fallback_element_type(db, env)
                        })
                }
            }
        };

        self.store_expression_type(target, loop_var_value_type);
        self.add_binding(target.into(), definition)
            .insert(self, loop_var_value_type);
    }

    fn infer_while_statement(&mut self, while_statement: &ast::StmtWhile) {
        let db = self.db();
        let ast::StmtWhile {
            range: _,
            node_index: _,
            test,
            body,
            orelse,
        } = while_statement;

        let test_ty = self.infer_standalone_expression(test, TypeContext::default());

        if let Err(err) = test_ty.try_bool(db, self.program_environment()) {
            err.report_diagnostic(&self.context, &**test);
        } else {
            self.check_condition(test);
        }

        self.infer_body(body);
        self.infer_body(orelse);
    }

    fn infer_assert_statement(&mut self, assert: &ast::StmtAssert) {
        let db = self.db();
        let ast::StmtAssert {
            range: _,
            node_index: _,
            test,
            msg,
        } = assert;

        let test_ty = self.infer_standalone_expression(test, TypeContext::default());

        if let Err(err) = test_ty.try_bool(db, self.program_environment()) {
            err.report_diagnostic(&self.context, &**test);
        } else {
            self.check_condition(test);
        }

        self.infer_optional_expression(msg.as_deref(), TypeContext::default());
    }

    fn infer_raise_statement(&mut self, raise: &ast::StmtRaise) {
        let db = self.db();
        let ast::StmtRaise {
            range: _,
            node_index: _,
            exc,
            cause,
        } = raise;

        let env = self.program_environment();
        let base_exception_type = KnownClass::BaseException.to_subclass_of(db, env);
        let base_exception_instance = KnownClass::BaseException.to_instance(db, env);

        let can_be_raised =
            UnionType::from_two_elements(db, env, base_exception_type, base_exception_instance);
        let can_be_exception_cause =
            UnionType::from_two_elements(db, env, can_be_raised, Type::none(db, env));

        if let Some(raised) = exc {
            let raised_type = self.infer_expression(raised, TypeContext::default());

            if !raised_type.is_assignable_to(db, env, can_be_raised) {
                report_invalid_exception_raised(&self.context, raised, raised_type);
            }
        }

        if let Some(cause) = cause {
            let cause_type = self.infer_expression(cause, TypeContext::default());

            if !cause_type.is_assignable_to(db, env, can_be_exception_cause) {
                report_invalid_exception_cause(&self.context, cause, cause_type);
            }
        }
    }

    fn infer_return_statement(&mut self, ret: &ast::StmtReturn) {
        let db = self.db();
        let env = self.program_environment();
        let tcx = if ret.value.is_some() {
            nearest_enclosing_function(db, self.index, self.scope())
                .map(|func| {
                    // When inferring expressions within a function body,
                    // the expected type passed should be the "raw" type,
                    // i.e. type variables in the return type are non-inferable,
                    // and the return types of async functions are not wrapped in `CoroutineType[...]`.
                    let return_ty = same_module_uncached_raw_signature(
                        db,
                        func,
                        ReturnCallableTypeVarScope::Lexical,
                    )
                    .return_ty;

                    // For generator functions, the declared return type is e.g.
                    // `Generator[YieldType, SendType, ReturnType]`. The type context
                    // for a `return` statement should be the `ReturnType` type parameter
                    let file_scope_id = self.scope().file_scope_id(self.db());
                    let context_ty = if file_scope_id.is_generator_function(self.index) {
                        return_ty
                            .generator_return_type(db, env)
                            .unwrap_or(return_ty)
                    } else {
                        return_ty
                    };

                    // basedpython: a `def` that wrote no return type has one recovered from this
                    // very body, and while that recovery is still running the signature answers
                    // with the cycle's own divergence marker. checking the returned expression
                    // against the marker makes the marker the answer: `def f(x: int): return [x]`
                    // reads its element type out of the context and comes back as
                    // `list[Divergent]`, which the next round reproduces unchanged, so the marker
                    // is what the iteration settles on and what a caller is shown.
                    //
                    // a marker is the cycle's stand-in for a type it has not reached yet, which is
                    // no guidance at all, so the expression is inferred bare and the round says
                    // what the body actually builds — `list[int]`, which the round after it is
                    // free to use as a context like any written annotation.
                    //
                    // only a return type nobody wrote down is dropped this way. an annotation is a
                    // constraint on the body however its own definition recurses: `-> dict[str,
                    // JsonValue]` where `JsonValue` is a recursive alias carries a marker of that
                    // alias's cycle, and dropping the context there leaves the returned display
                    // without the bidirectional inference the annotation exists to give — which
                    // reads back as a return type that does not fit the one written
                    let declares_return_type = self
                        .index
                        .scope(file_scope_id)
                        .node()
                        .as_function()
                        .is_some_and(|function| function.node(self.module()).returns.is_some());
                    if !declares_return_type && context_ty.mentions_divergence(db, env) {
                        return TypeContext::default();
                    }

                    TypeContext::new(Some(context_ty))
                })
                .unwrap_or_default()
        } else {
            TypeContext::default()
        };
        if let Some(ty) = self.infer_optional_expression(ret.value.as_deref(), tcx) {
            let range = ret
                .value
                .as_ref()
                .map_or(ret.range(), |value| value.range());
            self.record_return_type(ty, range);
        } else {
            self.record_return_type(Type::none(db, env), ret.range());
        }
    }

    fn infer_delete_statement(&mut self, delete: &ast::StmtDelete) {
        let ast::StmtDelete {
            range: _,
            node_index: _,
            targets,
        } = delete;
        for target in targets {
            self.infer_expression(target, TypeContext::default());
        }
    }

    fn infer_global_statement(&mut self, global: &ast::StmtGlobal) {
        // CPython allows examples like this, where a global variable is never explicitly defined
        // in the global scope:
        //
        // ```py
        // def f():
        //     global x
        //     x = 1
        // def g():
        //     print(x)
        // ```
        //
        // However, allowing this pattern would make it hard for us to guarantee
        // accurate analysis about the types and boundness of global-scope symbols,
        // so we require the variable to be explicitly defined (either bound or declared)
        // in the global scope.
        let ast::StmtGlobal {
            node_index: _,
            range: _,
            names,
        } = global;
        let global_place_table = self.index.place_table(FileScopeId::global());
        for name in names {
            if let Some(symbol_id) = global_place_table.symbol_id(name) {
                let symbol = global_place_table.symbol(symbol_id);
                if symbol.is_bound() || symbol.is_declared() {
                    // This name is explicitly defined in the global scope (not just in function
                    // bodies that mark it `global`).
                    continue;
                }
            }
            if !module_type_implicit_global_symbol(self.db(), self.program_file(), name)
                .place
                .is_undefined()
            {
                // This name is an implicit global like `__file__` (but not a built-in like `int`).
                continue;
            }
            // This variable isn't explicitly defined in the global scope, nor is it an
            // implicit global from `types.ModuleType`, so we consider this `global` statement invalid.
            let Some(builder) = self.context.report_lint(&UNRESOLVED_GLOBAL, name) else {
                return;
            };
            let mut diag =
                builder.into_diagnostic(format_args!("Invalid global declaration of `{name}`"));
            diag.set_primary_annotation_message(format_args!(
                "`{name}` has no declarations or bindings in the global scope"
            ));
            diag.info(
                "This limits ty's ability to make accurate inferences \
                about the boundness and types of global-scope symbols",
            );
            diag.info(format_args!(
                "Consider adding a declaration to the global scope, e.g. `{name}: int`"
            ));
        }
    }

    fn module_type_from_name(&self, module_name: &ModuleName) -> Option<Type<'db>> {
        let db = self.db();
        let importing_file = ImportingFile::File(
            self.file(),
            self.program_environment().resolver_environment(db),
        );
        resolve_module(db, importing_file, module_name)
            .map(|module| Type::module_literal(self.db(), self.program_file(), module))
    }

    fn infer_decorator(&mut self, decorator: &ast::Decorator) -> Type<'db> {
        let env = self.program_environment();
        let ast::Decorator {
            range: _,
            node_index: _,
            expression,
        } = decorator;

        // basedpython modifier keywords (`final def`, `abstract def`, ...) parse as
        // synthetic decorators whose source text starts with the keyword letter
        // instead of `@`. resolve them to the equivalent stdlib decorator type so
        // downstream type checking treats them like the user wrote `@typing.final`
        if let Some(target) = crate::types::function::synthetic_decorator_target_type(
            self.db(),
            env,
            self.file(),
            decorator,
        ) {
            self.store_expression_type(expression, target);
            return target;
        }

        let source = source_text(self.db(), self.file());
        let start = usize::from(decorator.range().start());
        if source.as_bytes().get(start).copied() != Some(b'@') {
            // basedpython: a property accessor block synthesizes `@<name>.setter`
            // with no `@` in the source. unlike a modifier keyword this *is* a real
            // attribute access — on the property the getter just built — so resolve
            // it instead of treating it as inert, which would erase the property
            if let ast::Expr::Attribute(attribute) = expression
                && matches!(attribute.attr.as_str(), "setter" | "getter" | "deleter")
            {
                return self.infer_expression(expression, TypeContext::default());
            }
            let ty = Type::unknown();
            self.store_expression_type(expression, ty);
            return ty;
        }

        self.infer_expression(expression, TypeContext::default())
    }

    /// Preserve the descriptor behavior of a transparent callable decorator when it is written
    /// as the equivalent assignment form in a class body.
    fn apply_desugared_decorator(
        &mut self,
        decorator_ty: Type<'db>,
        call_expression: &ast::ExprCall,
        return_ty: Type<'db>,
    ) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let arguments = &call_expression.arguments;
        let [decorated_expression] = &arguments.args[..] else {
            return return_ty;
        };
        if !arguments.keywords.is_empty() || decorated_expression.is_starred_expr() {
            return return_ty;
        }

        let decorated_ty =
            self.get_or_infer_expression(decorated_expression, TypeContext::default());
        let call_arguments = CallArguments::positional([decorated_ty]);
        let Ok(bindings) = decorator_ty.try_call(db, env, &call_arguments) else {
            return return_ty;
        };

        transparent_callable_decorator_result(db, env, &bindings, decorated_ty).unwrap_or(return_ty)
    }

    /// basedpython: infer the decorators written above a binding.
    ///
    /// A decorator on a binding is metadata on its declared type — the lowering puts
    /// it in the `Annotated` the annotation becomes — so it does not change what the
    /// binding holds, and the type is handed straight back. What this is for is the
    /// decorators themselves: each is an ordinary expression, and inferring it is
    /// where an unresolved name in one is reported, and what gives the decorator node
    /// a type for the IDE to read.
    fn infer_binding_decorators(
        &mut self,
        decorators: &'ast [ast::Decorator],
        ty: Type<'db>,
    ) -> Type<'db> {
        for decorator in decorators {
            self.infer_decorator(decorator);
        }
        ty
    }

    /// Apply a decorator to a function or class type and return the resulting type.
    ///
    /// Constructor semantics for class-like decorators are handled by `Type::bindings`, so we
    /// can always use `try_call` here.
    fn apply_decorator(
        &mut self,
        decorator_ty: Type<'db>,
        decorated_ty: Type<'db>,
        decorator_node: &ast::Decorator,
    ) -> Type<'db> {
        fn propagate_callable_kind<'d>(
            db: &'d dyn Db,
            env: &ProgramEnvironment<'d>,
            ty: Type<'d>,
            kind: CallableTypeKind,
            provenance: CallableFunctionProvenance,
        ) -> Option<Type<'d>> {
            match ty {
                // parameter-only marker; behaves as the type a body sees (bound of `Key`)
                Type::Overlapping(overlapping) => propagate_callable_kind(
                    db,
                    env,
                    overlapping.value_type(db, env),
                    kind,
                    provenance,
                ),
                Type::Restricted(restricted) => {
                    propagate_callable_kind(db, env, restricted.value_type(db), kind, provenance)
                }
                Type::Deferred(deferred) => {
                    propagate_callable_kind(db, env, deferred.reduced(db, env), kind, provenance)
                }
                Type::Callable(callable) => Some(Type::Callable(CallableType::new(
                    db,
                    callable.signatures(db),
                    kind,
                    provenance,
                ))),
                Type::Union(union) => union.try_map(db, env, |element| {
                    propagate_callable_kind(db, env, *element, kind, provenance)
                }),
                Type::TypeAlias(alias) => {
                    propagate_callable_kind(db, env, alias.value_type(db), kind, provenance)
                }
                // Intersections are currently not handled here because that would require
                // the decorator to be explicitly annotated as returning an intersection.
                Type::Intersection(_) | Type::EnumComplement(_) | Type::UnsafeUnion(_) => None,
                // All other types cannot have a callable kind propagated to them.
                Type::Dynamic(_)
                | Type::Divergent(_)
                | Type::Never
                | Type::FunctionLiteral(_)
                | Type::BoundMethod(_)
                | Type::KnownBoundMethod(_)
                | Type::WrapperDescriptor(_)
                | Type::DataclassDecorator(_)
                | Type::DataclassTransformer(_)
                | Type::ModuleLiteral(_)
                | Type::ClassLiteral(_)
                | Type::GenericAlias(_)
                | Type::SubclassOf(_)
                | Type::NominalInstance(_)
                | Type::ProtocolInstance(_)
                | Type::SpecialForm(_)
                | Type::KnownInstance(_)
                | Type::PropertyInstance(_)
                | Type::AlwaysTruthy
                | Type::AlwaysFalsy
                | Type::LiteralValue(_)
                | Type::TypeVar(_)
                | Type::BoundSuper(_)
                | Type::TypeIs(_)
                | Type::TypeGuard(_)
                | Type::TypeForm(_)
                | Type::TypedDict(_)
                | Type::NewTypeInstance(_) => None,
            }
        }
        let db = self.db();

        let env = self.program_environment();
        // For FunctionLiteral, get the kind directly without computing the full signature.
        // This avoids a query cycle when the function has default parameter values, since
        // computing the signature requires evaluating those defaults which may trigger
        // deferred inference.
        let propagatable_kind = match decorated_ty {
            Type::FunctionLiteral(func) => Some((
                func.callable_type_kind(self.db()),
                CallableFunctionProvenance::from_function_return_annotation(
                    func.has_explicit_return_annotation(self.db()),
                ),
            )),
            _ => decorated_ty
                .try_upcast_to_callable(db, env)
                .and_then(CallableTypes::exactly_one)
                .and_then(|callable| match callable.kind(self.db()) {
                    kind @ (CallableTypeKind::FunctionLike
                    | CallableTypeKind::StaticMethodLike
                    | CallableTypeKind::ClassMethodLike) => {
                        Some((kind, callable.provenance(self.db())))
                    }
                    _ => None,
                }),
        };

        let call_arguments = CallArguments::positional([decorated_ty]);
        let (return_ty, decorator_bindings) = match decorator_ty.try_call(db, env, &call_arguments)
        {
            Ok(bindings) => (bindings.return_type(db, env), Some(bindings)),
            Err(CallError(_, bindings)) => {
                bindings.report_diagnostics(&self.context, decorator_node.into());
                (bindings.return_type(db, env), None)
            }
        };

        // TODO: Remove this special case once the new constraint solver can preserve
        // per-overload ParamSpec/return correlations for transparent callable decorators.
        if let Some(decorator_bindings) = decorator_bindings.as_ref()
            && let Some(result) =
                transparent_callable_decorator_result(db, env, decorator_bindings, decorated_ty)
        {
            return result;
        }

        // When a method on a class is decorated with a function that returns a
        // `Callable`, assume that the returned callable is also function-like (or
        // classmethod-like or staticmethod-like). See "Decorating a method with
        // a `Callable`-typed decorator" in `callables_as_descriptors.md` for the
        // extended explanation.
        propagatable_kind
            .and_then(|(kind, provenance)| {
                propagate_callable_kind(db, env, return_ty, kind, provenance)
            })
            .unwrap_or(return_ty)
    }

    #[expect(clippy::too_many_arguments)]
    fn infer_and_try_call_dunder(
        &mut self,
        object: Type<'db>,
        name: &str,
        lookup_policy: MemberLookupPolicy,
        ast_arguments: ArgumentsIter<'_>,
        argument_types: &mut CallArguments<'_, 'db>,
        infer_argument_ty: &mut dyn FnMut(&mut Self, ArgExpr<'db, '_>) -> Type<'db>,
        call_expression_tcx: TypeContext<'db>,
    ) -> Result<Bindings<'db>, CallDunderError<'db>> {
        let db = self.db();
        let env = self.program_environment();
        match object
            .member_lookup_with_policy(db, env, name, lookup_policy)
            .place
        {
            Place::Defined(DefinedPlace {
                ty: dunder_callable,
                definedness: boundness,
                provenance,
                ..
            }) => {
                let mut bindings = self.bindings_for_call(dunder_callable).match_parameters(
                    db,
                    env,
                    argument_types,
                );

                if let Err(call_error) = self.infer_and_check_argument_types(
                    ast_arguments,
                    argument_types,
                    infer_argument_ty,
                    &mut bindings,
                    call_expression_tcx,
                ) {
                    return Err(CallDunderError::CallError(
                        call_error,
                        Box::new(bindings),
                        provenance,
                    ));
                }

                if boundness == Definedness::PossiblyUndefined {
                    return Err(CallDunderError::PossiblyUnbound {
                        bindings: Box::new(bindings),
                        unbound_on: None,
                    });
                }
                Ok(bindings)
            }
            Place::Undefined => Err(CallDunderError::MethodNotAvailable),
        }
    }

    fn infer_and_check_argument_types(
        &mut self,
        ast_arguments: ArgumentsIter<'_>,
        argument_types: &mut CallArguments<'_, 'db>,
        infer_argument_ty: &mut dyn FnMut(&mut Self, ArgExpr<'db, '_>) -> Type<'db>,
        bindings: &mut Bindings<'db>,
        call_expression_tcx: TypeContext<'db>,
    ) -> Result<(), CallErrorKind> {
        let db = self.db();
        let constraints = ConstraintSetBuilder::new();
        let initial_argument_types = argument_types.clone();
        let env = self.program_environment();

        // Keep track of which arguments match generic parameters.
        let mut generic_arguments = SmallVec::<[bool; 8]>::with_capacity(argument_types.len());
        generic_arguments.resize(argument_types.len(), false);

        let mut max_typevar_occurrences = 0;
        let mut has_generic_context = false;
        let mut overload_candidates = OverloadSet::new();

        // Compute the upper bound on fixpoint iteration, based on the maximum number of inferable
        // typevar occurrences across all overload candidates. Note that the set of overload candidates
        // stays stable across all iterations.
        bindings.visit_type_context_callables(&mut |binding| {
            let candidate_overload_indices =
                binding.candidate_overload_indices(db, env, argument_types);

            has_generic_context |= candidate_overload_indices.iter().any(|&overload_index| {
                binding.overloads()[overload_index]
                    .signature
                    .generic_context
                    .is_some()
            });

            for overload_index in &candidate_overload_indices {
                let overload = &binding.overloads()[*overload_index];
                if overload.signature.generic_context.is_none() {
                    continue;
                }

                let mut overload_typevar_occurrences = 0;
                for (argument_index, is_generic) in generic_arguments.iter_mut().enumerate() {
                    if argument_types.is_variadic(argument_index) {
                        continue;
                    }

                    let typevar_occurrences = overload.typevar_occurrences_for_parameter(
                        db,
                        env,
                        binding,
                        argument_index,
                    );
                    *is_generic |= typevar_occurrences > 0;
                    overload_typevar_occurrences += typevar_occurrences;
                }

                max_typevar_occurrences = max_typevar_occurrences.max(overload_typevar_occurrences);
            }

            overload_candidates.push(candidate_overload_indices);
        });

        let generic_arguments: SmallVec<_> = generic_arguments
            .into_iter()
            .enumerate()
            .filter_map(|(index, is_generic)| is_generic.then_some(index))
            .collect();

        // Enable the expression cache if we are going to perform multi-inference.
        let teardown_expression_cache = if !generic_arguments.is_empty()
            || requires_overload_evaluation(&overload_candidates)
        {
            self.setup_expression_cache()
        } else {
            false
        };

        // If the type context is a union, attempt to narrow to a specific element.
        let narrow_targets = call_expression_tcx
            .narrow_targets(db, env)
            // We only need to attempt narrowing on generic calls, otherwise the type
            // context has no effect.
            .filter(|_| has_generic_context)
            .unwrap_or_default();

        let mut try_narrow = |narrowed_ty: Type<'db>| {
            // Short-circuit if there is no overload with a matching return type.
            if !bindings.satisfies(|overload| {
                let inferable = overload
                    .signature
                    .generic_context
                    .map(|generic_context| generic_context.inferable_typevars(db))
                    .unwrap_or(TypeVarSet::None);

                !overload
                    .return_ty
                    .when_assignable_to(db, env, narrowed_ty, &constraints, inferable)
                    .is_never_satisfied(db, env)
            }) {
                return None;
            }

            let narrowed_tcx = TypeContext::new(Some(narrowed_ty));

            let mut speculative_bindings = bindings.clone();
            let mut speculative_builder = self.speculate();
            let mut speculative_argument_types = initial_argument_types.clone();

            // Attempt to infer the argument types using the narrowed type context.
            //
            // If there are matching generic parameters on any overload, we perform fixpoint
            // iteration to allow call arguments to contribute type context constraints to
            // other siblings.
            let result = if !generic_arguments.is_empty() {
                speculative_builder.infer_and_check_argument_types_unified(
                    &ast_arguments,
                    &mut speculative_argument_types,
                    infer_argument_ty,
                    &mut speculative_bindings,
                    &constraints,
                    narrowed_tcx,
                    &generic_arguments,
                    max_typevar_occurrences,
                    &overload_candidates,
                )
            } else {
                speculative_builder.infer_and_check_argument_types_simple(
                    ast_arguments.clone(),
                    &mut speculative_argument_types,
                    &initial_argument_types,
                    infer_argument_ty,
                    &mut speculative_bindings,
                    &constraints,
                    narrowed_tcx,
                    &overload_candidates,
                )
            };

            if result.is_err() {
                return None;
            }

            // Ensure the inferred return type is assignable to the narrowed declared type.
            //
            // TODO: Checking assignability against the full declared type could help avoid
            // cases where the constraint solver is not smart enough to solve complex unions.
            // We should see revisit this after the new constraint solver is implemented.
            if !speculative_bindings
                .return_type(db, env)
                .is_assignable_to(db, env, narrowed_ty)
            {
                return None;
            }

            // Successfully narrowed to an element of the union.
            *bindings = speculative_bindings;
            *argument_types = speculative_argument_types;
            self.extend(speculative_builder);

            Some(result)
        };

        // Prefer the declared type of generic classes or callables when narrowing.
        //
        // Splitting up this loop is not necessary for correctness, but leads to a slight
        // performance improvement.
        for narrowed_ty in std::iter::chain(
            narrow_targets
                .iter()
                .filter(|ty| ty.may_prefer_declared_type(db, env)),
            narrow_targets
                .iter()
                .filter(|ty| !ty.may_prefer_declared_type(db, env)),
        ) {
            if let Some(result) = try_narrow(*narrowed_ty) {
                if teardown_expression_cache {
                    self.teardown_expression_cache();
                }

                return result;
            }
        }

        *argument_types = initial_argument_types.clone();

        // Infer against the entire union as a fallback.
        //
        // TODO: We could also attempt an inference without type context, but this
        // leads to similar performance issues.
        let result = if !generic_arguments.is_empty() {
            self.infer_and_check_argument_types_unified(
                &ast_arguments,
                argument_types,
                infer_argument_ty,
                bindings,
                &constraints,
                call_expression_tcx,
                &generic_arguments,
                max_typevar_occurrences,
                &overload_candidates,
            )
        } else {
            self.infer_and_check_argument_types_simple(
                ast_arguments,
                argument_types,
                &initial_argument_types,
                infer_argument_ty,
                bindings,
                &constraints,
                call_expression_tcx,
                &overload_candidates,
            )
        };

        if teardown_expression_cache {
            self.teardown_expression_cache();
        }

        result
    }

    #[expect(clippy::too_many_arguments)]
    fn infer_and_check_argument_types_simple<'call>(
        &mut self,
        ast_arguments: ArgumentsIter<'_>,
        argument_types: &mut CallArguments<'call, 'db>,
        baseline_argument_types: &CallArguments<'call, 'db>,
        infer_argument_ty: &mut dyn FnMut(&mut Self, ArgExpr<'db, '_>) -> Type<'db>,
        bindings: &mut Bindings<'db>,
        constraints: &ConstraintSetBuilder<'db>,
        call_expression_tcx: TypeContext<'db>,
        candidates: &OverloadSet,
    ) -> Result<(), CallErrorKind> {
        let db = self.db();
        let env = self.program_environment();
        let requires_overload_evaluation = requires_overload_evaluation(candidates);
        let arguments_tcx = self.collect_call_arguments_type_context(
            baseline_argument_types,
            bindings,
            requires_overload_evaluation.then_some(candidates),
            constraints,
            call_expression_tcx,
        );

        // If we are not inferring against multiple overloads, we can infer the arguments
        // and check the binding directly.
        if !requires_overload_evaluation {
            self.infer_all_argument_types(
                ast_arguments,
                argument_types,
                &arguments_tcx,
                infer_argument_ty,
                CallArgumentInferenceMode::Commit,
            );

            return bindings.check_types_impl(
                db,
                env,
                constraints,
                argument_types,
                call_expression_tcx,
                &self.dataclass_field_specifiers,
                CheckTypesMode::Finalize,
            );
        }

        // Otherwise, we first infer the argument types speculatively.
        let mut speculative_builder = self.speculate();
        speculative_builder.infer_all_argument_types(
            ast_arguments.clone(),
            argument_types,
            &arguments_tcx,
            infer_argument_ty,
            // If there are multiple matching overloads, we will re-infer with the final set
            // of matching overloads after overload evaluation, and so can avoid the default
            // inference here.
            CallArgumentInferenceMode::Speculate,
        );

        let result = bindings.check_types_impl(
            db,
            env,
            constraints,
            argument_types,
            call_expression_tcx,
            &self.dataclass_field_specifiers,
            CheckTypesMode::Finalize,
        );

        let checked_argument_types = argument_types.clone();
        *argument_types = baseline_argument_types.clone();

        // And re-infer argument types after overload evaluation, ensuring that only
        // inferred types and diagnostics from matching overloads are preserved.
        let arguments_tcx = self.collect_call_arguments_type_context(
            &checked_argument_types,
            bindings,
            None,
            constraints,
            call_expression_tcx,
        );
        self.infer_all_argument_types(
            ast_arguments,
            argument_types,
            &arguments_tcx,
            infer_argument_ty,
            CallArgumentInferenceMode::Commit,
        );
        self.union_expected_types(&speculative_builder.expected_types);

        result
    }

    /// Infer generic call arguments under fixpoint iteration, allowing arguments to contribute
    /// type context constraints to other siblings.
    #[expect(clippy::too_many_arguments)]
    fn infer_and_check_argument_types_unified(
        &mut self,
        ast_arguments: &ArgumentsIter<'_>,
        argument_types: &mut CallArguments<'_, 'db>,
        infer_argument_ty: &mut dyn FnMut(&mut Self, ArgExpr<'db, '_>) -> Type<'db>,
        bindings: &mut Bindings<'db>,
        constraints: &ConstraintSetBuilder<'db>,
        call_expression_tcx: TypeContext<'db>,
        generic_arguments: &SmallVec<[usize; 4]>,
        typevar_occurrences: usize,
        candidates: &OverloadSet,
    ) -> Result<(), CallErrorKind> {
        let db = self.db();
        let requires_overload_evaluation = requires_overload_evaluation(candidates);

        let mut arguments_tcx = self.collect_call_arguments_type_context(
            argument_types,
            bindings,
            Some(candidates),
            constraints,
            call_expression_tcx,
        );

        let mut iteration = 0;
        let mut next_bindings = bindings.clone();
        let mut prev_argument_types = argument_types.clone();

        let (converged_builder, converged_argument_types) = loop {
            let mut next_argument_types = argument_types.clone();

            // Infer the argument types for the current iteration.
            let mut speculative_builder = self.speculate();
            speculative_builder.infer_all_argument_types(
                ast_arguments.clone(),
                &mut next_argument_types,
                &arguments_tcx,
                infer_argument_ty,
                if requires_overload_evaluation {
                    // If there are multiple matching overloads, we will re-infer with the final set
                    // of matching overloads after overload evaluation, and so can avoid the default
                    // inference here.
                    CallArgumentInferenceMode::Speculate
                } else {
                    CallArgumentInferenceMode::Commit
                },
            );

            let inferred_types_converged = next_argument_types
                .inferred_types_equal_at(&prev_argument_types, generic_arguments);

            // If the inferred types have converged, and already evaluated the bindings from the
            // previous iteration, we are done.
            if iteration > 0 && inferred_types_converged {
                break (speculative_builder, next_argument_types);
            }

            // Otherwise, we have to evaluate the bindings against the newly inferred types.
            next_bindings = bindings.clone();
            let _ = next_bindings.check_types_impl(
                db,
                self.program_environment(),
                constraints,
                &next_argument_types,
                call_expression_tcx,
                &self.dataclass_field_specifiers,
                CheckTypesMode::Provisional,
            );

            // The number of occurrences of inferable typevars forms an upper bound for the number
            // of fixpoint iterations, and so if the types have converged, or we have reached the
            // upper bound, we are done.
            if inferred_types_converged || iteration == typevar_occurrences {
                break (speculative_builder, next_argument_types);
            }

            // Collect the argument constraints based on the newly inferred types.
            let next_arguments_tcx = self.collect_call_arguments_type_context(
                &next_argument_types,
                &next_bindings,
                Some(candidates),
                constraints,
                call_expression_tcx,
            );

            // If the argument constraints have converged, the inferred types will be identical,
            // and so we can exit early.
            if generic_arguments
                .iter()
                .all(|&index| arguments_tcx.get(index) == next_arguments_tcx.get(index))
            {
                break (speculative_builder, next_argument_types);
            }

            iteration += 1;
            arguments_tcx = next_arguments_tcx;
            prev_argument_types = next_argument_types;
        };

        // Discard any non-matching constructors overloads now that the inferred types have converged.
        let result = next_bindings.finalize_argument_inference(
            db,
            self.program_environment(),
            &converged_argument_types,
            &self.dataclass_field_specifiers,
        );

        // If the set of candidate bindings contained multiple matching overloads, re-infer the argument
        // types against the final set of matching overloads, such that only the relevant diagnostics
        // and inferred types are preserved.
        if requires_overload_evaluation {
            let arguments_tcx = self.collect_call_arguments_type_context(
                &converged_argument_types,
                &next_bindings,
                None,
                constraints,
                call_expression_tcx,
            );

            self.infer_all_argument_types(
                ast_arguments.clone(),
                argument_types,
                &arguments_tcx,
                infer_argument_ty,
                CallArgumentInferenceMode::Commit,
            );

            self.union_expected_types(&converged_builder.expected_types);
        } else {
            // Otherwise, we can simply use the newly inferred types.
            *argument_types = converged_argument_types;
            self.extend(converged_builder);
        }

        *bindings = next_bindings;
        result
    }

    /// Collects the type contexts used to infer the arguments of a call expression.
    fn collect_call_arguments_type_context<'bindings>(
        &self,
        argument_types: &CallArguments<'_, 'db>,
        bindings: &'bindings Bindings<'db>,
        candidates: Option<&'bindings OverloadSet>,
        constraints: &ConstraintSetBuilder<'db>,
        call_expression_tcx: TypeContext<'db>,
    ) -> Vec<Option<MatchingArgumentTypeContext<'db>>> {
        type OverloadsWithBinding<'a, 'db> = Vec<(
            &'a Binding<'db>,
            &'a CallableBinding<'db>,
            Option<Specialization<'db>>,
        )>;

        fn add_overloads_from_binding<'a, 'db>(
            db: &'db dyn Db,
            file: ruff_db::files::File,
            env: &ProgramEnvironment<'db>,
            overloads_with_binding: &mut OverloadsWithBinding<'a, 'db>,
            binding: &'a CallableBinding<'db>,
            constraints: &ConstraintSetBuilder<'db>,
            call_expression_tcx: TypeContext<'db>,
        ) {
            let mut matching_overloads = binding.matching_overloads().peekable();
            if matching_overloads.peek().is_some() {
                overloads_with_binding.extend(matching_overloads.map(|(_, overload)| {
                    let specialization = overload.argument_type_context_specialization(
                        db,
                        file,
                        env,
                        constraints,
                        call_expression_tcx,
                    );

                    (overload, binding, specialization)
                }));
            } else if let Some(overload) = binding.best_failing_overload() {
                let specialization = overload.argument_type_context_specialization(
                    db,
                    file,
                    env,
                    constraints,
                    call_expression_tcx,
                );

                // If there is a single overload that does not match, we still infer the argument
                // types for better diagnostics.
                overloads_with_binding.push((overload, binding, specialization));
            }
        }
        let db = self.db();
        let file = self.file();

        let env = self.program_environment();

        // Collect the set of candidate overloads and bindings.
        let mut overloads_with_binding: OverloadsWithBinding = Vec::new();
        if let Some(candidates) = candidates {
            bindings.visit_overload_set(candidates, &mut |overload, binding| {
                let specialization = overload.argument_type_context_specialization(
                    db,
                    file,
                    env,
                    constraints,
                    call_expression_tcx,
                );

                overloads_with_binding.push((overload, binding, specialization));
            });
        } else {
            bindings.visit_type_context_callables(&mut |binding| {
                add_overloads_from_binding(
                    db,
                    file,
                    env,
                    &mut overloads_with_binding,
                    binding,
                    constraints,
                    call_expression_tcx,
                );
            });
        }

        // Collect the type context of each argument from each matching overload.
        (0..argument_types.len())
            .map(|argument_index| {
                if argument_types.is_variadic(argument_index) {
                    return None;
                }

                let parameter_tcx =
                    |overload: &Binding<'db>, binding: &CallableBinding<'db>, specialization| {
                        overload.argument_type_context(
                            db,
                            env,
                            constraints,
                            binding,
                            argument_types,
                            argument_index,
                            call_expression_tcx,
                            specialization,
                        )
                    };

                let parameter_contexts = if let Ok((overload, binding, specialization)) =
                    overloads_with_binding.iter().exactly_one()
                {
                    MatchingArgumentTypeContext::Unique(parameter_tcx(
                        overload,
                        binding,
                        *specialization,
                    ))
                } else {
                    MatchingArgumentTypeContext::Many(
                        overloads_with_binding
                            .iter()
                            .map(|(overload, binding, specialization)| {
                                parameter_tcx(overload, binding, *specialization)
                            })
                            .collect(),
                    )
                };

                Some(parameter_contexts)
            })
            .collect()
    }

    /// Infers every call argument using the provided set of type context.
    fn infer_all_argument_types(
        &mut self,
        ast_arguments: ArgumentsIter<'_>,
        argument_types: &mut CallArguments<'_, 'db>,
        arguments_tcx: &[Option<MatchingArgumentTypeContext<'db>>],
        infer_argument_ty: &mut dyn FnMut(&mut Self, ArgExpr<'db, '_>) -> Type<'db>,
        mode: CallArgumentInferenceMode,
    ) {
        let insert_argument_ty =
            |argument_index,
             inferred_ty,
             argument_tcx: &Option<ArgumentTypeContext<'db>>,
             argument_types: &mut CallArguments<'_, 'db>| {
                if let Some(argument_tcx) = argument_tcx {
                    argument_tcx.insert_inferred_type_into(
                        argument_types,
                        argument_index,
                        inferred_ty,
                    );
                } else {
                    argument_types.insert_type(argument_index, TypeContext::default(), inferred_ty);
                }
            };

        for (argument_index, ast_argument) in ast_arguments.enumerate() {
            // Splatted arguments are inferred before parameter matching to
            // determine their length.
            //
            // TODO: Re-infer splatted arguments with their type context.
            if ast_argument.is_variadic() {
                continue;
            }
            let ast_argument = ast_argument.value();

            let Some(argument_tcx) = &arguments_tcx[argument_index] else {
                continue;
            };

            match argument_tcx {
                MatchingArgumentTypeContext::Unique(argument_tcx) => {
                    let tcx = argument_tcx
                        .map(ArgumentTypeContext::type_context)
                        .unwrap_or_default();
                    let inferred_ty = infer_argument_ty(self, (argument_index, ast_argument, tcx));
                    insert_argument_ty(argument_index, inferred_ty, argument_tcx, argument_types);
                }

                MatchingArgumentTypeContext::Many(argument_tcx) => {
                    let mut inferred_by_cache_key = FxHashMap::default();

                    // If there are multiple applicable type contexts and we are not in
                    // speculative mode, infer the argument without type context as the
                    // default inference.
                    if mode.requires_default_inference() {
                        let inferred_ty = infer_argument_ty(
                            self,
                            (argument_index, ast_argument, TypeContext::default()),
                        );

                        argument_types.insert_type(
                            argument_index,
                            TypeContext::default(),
                            inferred_ty,
                        );

                        inferred_by_cache_key.insert(None, inferred_ty);
                    }

                    // Cache expressions inferred across speculative inference attempts.
                    //
                    // This is important to avoid exponential blowup for deeply nested generic calls,
                    // as inner expressions are repeatedly inferred with the same type context.
                    let teardown_expression_cache = self.setup_expression_cache();

                    for argument_tcx in argument_tcx {
                        let inference_cache_key =
                            argument_tcx.map(ArgumentTypeContext::inference_cache_key);
                        if let Some(inferred_ty) =
                            inferred_by_cache_key.get(&inference_cache_key).copied()
                        {
                            // Even when the inference cache key is identical, this overload may later
                            // look up the inferred type through a different original `ParamSpec`
                            // annotation, so insert through its own context.
                            insert_argument_ty(
                                argument_index,
                                inferred_ty,
                                argument_tcx,
                                argument_types,
                            );

                            continue;
                        }

                        let tcx = argument_tcx
                            .map(ArgumentTypeContext::type_context)
                            .unwrap_or_default();

                        let mut speculative_builder = self.speculate();
                        let inferred_ty = infer_argument_ty(
                            &mut speculative_builder,
                            (argument_index, ast_argument, tcx),
                        );

                        insert_argument_ty(
                            argument_index,
                            inferred_ty,
                            argument_tcx,
                            argument_types,
                        );

                        inferred_by_cache_key.insert(inference_cache_key, inferred_ty);
                        self.union_expected_types(&speculative_builder.expected_types);
                    }

                    if teardown_expression_cache {
                        self.teardown_expression_cache();
                    }
                }
            }
        }
    }

    fn infer_maybe_standalone_statement(&mut self, statement: &ast::Stmt) {
        if let Some(standalone_statement) = self.index.try_statement(statement) {
            self.infer_standalone_statement_impl(standalone_statement);
        } else {
            self.infer_statement(statement);
        }
    }

    fn infer_standalone_statement_impl(&mut self, standalone_statement: Statement<'db>) {
        let types = infer_statement_types(self.db(), standalone_statement);
        self.extend_statement(&types);
    }

    fn infer_optional_expression(
        &mut self,
        expression: Option<&ast::Expr>,
        tcx: TypeContext<'db>,
    ) -> Option<Type<'db>> {
        expression.map(|expr| self.infer_expression(expr, tcx))
    }

    #[track_caller]
    fn infer_expression(&mut self, expression: &ast::Expr, tcx: TypeContext<'db>) -> Type<'db> {
        debug_assert!(
            !self.index.is_standalone_expression(expression),
            "Calling `self.infer_expression` on a standalone-expression \
            is not allowed because it can lead to double-inference. \
            Use `self.infer_standalone_expression` instead."
        );

        self.infer_expression_impl(expression, tcx)
    }

    fn infer_expression_with_state(
        &mut self,
        expression: &ast::Expr,
        tcx: TypeContext<'db>,
        state: DeferredExpressionState,
    ) -> Type<'db> {
        let previous_deferred_state = std::mem::replace(&mut self.deferred_state, state);
        let ty = self.infer_expression(expression, tcx);
        self.deferred_state = previous_deferred_state;
        ty
    }

    fn infer_maybe_standalone_expression(
        &mut self,
        expression: &ast::Expr,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        if let Some(standalone_expression) = self.index.try_expression(expression) {
            self.infer_standalone_expression_impl(expression, standalone_expression, tcx)
        } else {
            self.infer_expression(expression, tcx)
        }
    }

    fn infer_expression_with_collection_literal_peer_context(
        &mut self,
        expression: &ast::Expr,
        tcx: TypeContext<'db>,
        peer_ty: Option<Type<'db>>,
    ) -> Type<'db> {
        self.infer_with_collection_literal_peer_context(expression, tcx, peer_ty, |builder, tcx| {
            builder.infer_expression(expression, tcx)
        })
    }

    fn infer_maybe_standalone_expression_with_collection_literal_peer_context(
        &mut self,
        expression: &ast::Expr,
        tcx: TypeContext<'db>,
        peer_ty: Option<Type<'db>>,
    ) -> Type<'db> {
        self.infer_with_collection_literal_peer_context(expression, tcx, peer_ty, |builder, tcx| {
            builder.infer_maybe_standalone_expression(expression, tcx)
        })
    }

    fn infer_with_collection_literal_peer_context(
        &mut self,
        expression: &ast::Expr,
        tcx: TypeContext<'db>,
        peer_ty: Option<Type<'db>>,
        mut infer_expression: impl FnMut(&mut Self, TypeContext<'db>) -> Type<'db>,
    ) -> Type<'db> {
        let peer_tcx = if is_empty_collection_type_context(tcx)
            && is_collection_literal(expression)
            && let Some(peer_ty) = peer_ty
        {
            TypeContext::new(Some(peer_ty))
        } else {
            return infer_expression(self, tcx);
        };

        let mut speculative_builder = self.speculate();
        let ty = infer_expression(&mut speculative_builder, peer_tcx);

        // Peer context is only an inference hint. If it introduces diagnostics, discard it and
        // infer normally so that only diagnostics intrinsic to the expression are reported.
        if speculative_builder.context.has_diagnostics() {
            infer_expression(self, tcx)
        } else {
            self.extend(speculative_builder);
            ty
        }
    }

    #[track_caller]
    fn infer_standalone_expression(
        &mut self,
        expression: &ast::Expr,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let standalone_expression = self.index.expression(expression);
        self.infer_standalone_expression_impl(expression, standalone_expression, tcx)
    }

    fn infer_standalone_expression_impl(
        &mut self,
        expression: &ast::Expr,
        standalone_expression: Expression<'db>,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let types = infer_expression_types(self.db(), standalone_expression, tcx);
        self.extend_expression(types);

        // Instead of calling `self.expression_type(expr)` after extending here, we get
        // the result from `types` directly because we might be in cycle recovery where
        // `types.cycle_fallback_type` is `Some(fallback_ty)`, which we can retrieve by
        // using `expression_type` on `types`:
        types.expression_type(expression)
    }

    /// Whether `expression`'s type may be shared through the multi-inference expression cache.
    ///
    /// A basedpython optional-chain link carries the present-receiver type that the rest of its
    /// chain resolves against out of band, in [`Self::basedpython_chain_present`], which the
    /// cache does not hold. Serving a link from the cache would strand the next link with the
    /// short-circuit `None`, and would do so only on whichever speculative attempt happened to
    /// miss the cache first.
    fn is_expression_cacheable(&self, expression: &ast::Expr) -> bool {
        !(self.is_basedpython_file() && is_basedpython_chain_link(expression))
    }

    /// Infer the type of an expression.
    fn infer_expression_impl(
        &mut self,
        expression: &ast::Expr,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let Some(expression_cache) = &self.expression_cache else {
            return self.infer_expression_uncached(expression, tcx);
        };

        if !self.is_expression_cacheable(expression) {
            return self.infer_expression_uncached(expression, tcx);
        }

        // See if we already have a cached entry for this expression.
        let expression_key = expression.into();
        let cache_entry = expression_cache.borrow().get(expression_key, tcx).cloned();

        match cache_entry {
            Some(ExpressionCacheEntry::Small(ty)) => {
                self.store_expression_type(expression, ty);
                ty
            }

            Some(ExpressionCacheEntry::Full(inference)) => {
                let ty = inference.expression_type(expression_key);
                self.extend_expression_cache_entry(&inference);
                ty
            }

            _ => {
                // The expression is uncached, infer it independently and cache the inference results.
                let mut speculative_builder = self.speculate();
                let ty = speculative_builder.infer_expression_uncached(expression, tcx);
                let inference = speculative_builder.into_expression_cache_entry();

                let cached = if inference.is_single_expression(expression_key, ty) {
                    self.store_expression_type(expression, ty);
                    ExpressionCacheEntry::Small(ty)
                } else {
                    self.extend_expression_cache_entry(&inference);
                    ExpressionCacheEntry::Full(Rc::new(inference))
                };

                if let Some(expression_cache) = &self.expression_cache {
                    expression_cache
                        .borrow_mut()
                        .insert(expression_key, tcx, cached);
                }

                ty
            }
        }
    }

    fn infer_expression_uncached(
        &mut self,
        expression: &ast::Expr,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        if let Some(target) = tcx.annotation()
            && let Some(ty) = self.infer_type_form_contextual_expression(expression, target)
        {
            self.store_expression_type(expression, ty);
            return ty;
        }

        self.infer_value_expression_impl(expression, tcx)
    }

    /// Infer an expression without implicitly treating this root as a `TypeForm`.
    ///
    /// Child expressions still use their normal contextual inference, so the
    /// expression's existing bidirectional behavior is preserved.
    fn infer_value_expression_impl(
        &mut self,
        expression: &ast::Expr,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let db = self.db();
        let mut ty = match expression {
            ast::Expr::NoneLiteral(ast::ExprNoneLiteral {
                range: _,
                node_index: _,
            }) => Type::none(db, self.program_environment()),
            ast::Expr::NumberLiteral(literal) => self.infer_number_literal_expression(literal),
            ast::Expr::BooleanLiteral(literal) => self.infer_boolean_literal_expression(literal),
            ast::Expr::StringLiteral(literal) => self.infer_string_literal_expression(literal, tcx),
            ast::Expr::BytesLiteral(bytes_literal) => {
                self.infer_bytes_literal_expression(bytes_literal)
            }
            ast::Expr::FString(fstring) => self.infer_fstring_expression(fstring),
            ast::Expr::TString(tstring) => self.infer_tstring_expression(tstring),
            ast::Expr::EllipsisLiteral(literal) => self.infer_ellipsis_literal_expression(literal),
            ast::Expr::Tuple(tuple) => self.infer_tuple_expression(tuple, tcx),
            ast::Expr::List(list) => self.infer_list_expression(list, tcx),
            ast::Expr::Set(set) => self.infer_set_expression(set, tcx),
            ast::Expr::Dict(dict) => self.infer_dict_expression(dict, tcx),
            ast::Expr::Generator(generator) => self.infer_generator_expression(generator, tcx),
            ast::Expr::ListComp(listcomp) => {
                self.infer_list_comprehension_expression(listcomp, tcx)
            }
            ast::Expr::DictComp(dictcomp) => {
                self.infer_dict_comprehension_expression(dictcomp, tcx)
            }
            ast::Expr::SetComp(setcomp) => self.infer_set_comprehension_expression(setcomp, tcx),
            ast::Expr::Name(name) => {
                let ty = self.infer_name_expression(name, tcx);
                tcx.annotation().map_or(ty, |target| {
                    self.specialize_generic_class_from_context(ty, target)
                })
            }
            ast::Expr::Attribute(attribute) => {
                let ty = self.infer_attribute_expression(attribute);
                tcx.annotation().map_or(ty, |target| {
                    self.specialize_generic_class_from_context(ty, target)
                })
            }
            ast::Expr::UnaryOp(unary_op) => self.infer_unary_expression(unary_op),
            ast::Expr::BinOp(binary) => self.infer_binary_expression(binary, tcx),
            ast::Expr::BoolOp(bool_op) => self.infer_boolean_expression(bool_op, tcx),
            ast::Expr::Compare(compare) => self.infer_compare_expression(compare),
            ast::Expr::Subscript(subscript) => self.infer_subscript_expression(subscript, tcx),
            ast::Expr::Slice(slice) => self.infer_slice_expression(slice),
            ast::Expr::If(if_expression) => self.infer_if_expression(if_expression, tcx),
            ast::Expr::Lambda(lambda_expression) => {
                self.infer_lambda_expression(lambda_expression, tcx)
            }
            ast::Expr::Call(call_expression) => self.infer_call_expression(call_expression, tcx),
            ast::Expr::Starred(starred) => self.infer_starred_expression(starred, tcx),
            ast::Expr::Yield(yield_expression) => self.infer_yield_expression(yield_expression),
            ast::Expr::YieldFrom(yield_from) => self.infer_yield_from_expression(yield_from),
            ast::Expr::Await(await_expression) => {
                self.infer_await_expression(await_expression, tcx)
            }
            ast::Expr::Named(named) => self.infer_named_expression(named),
            ast::Expr::IpyEscapeCommand(_) => {
                todo_type!("Ipy escape command support")
            }
            ast::Expr::CallableType(_) => {
                // callable-type syntax only valid in annotation position; in value context
                // the type expression inference path is responsible
                todo_type!("CallableType in value context")
            }
            ast::Expr::ProtocolType(_) | ast::Expr::ProtocolMethod(_) => {
                // inline protocol syntax only valid in annotation position; in value context
                // the type expression inference path is responsible
                todo_type!("inline protocol type in value context")
            }
            ast::Expr::Statement(statement) => self.infer_statement_expression(statement),
        };

        ty = self.apply_type_context(ty, tcx);

        if self.fluid_specializations_enabled()
            && let Some(candidate_def) = self.index.fluid_candidate_binding(expression)
        {
            if !tcx.inferred_from_argument
                && let Some(annotation) = tcx.annotation()
            {
                self.fluid_adoptions.insert(expression.into(), annotation);
            }

            // Uses of a fluid specialization candidate are typed flow-sensitively:
            // each use solves the specialization from the events that can have
            // executed before it, adopting any bidirectional type context that
            // constrains the specialization.
            if expression.is_name_expr() {
                ty = self.fluid_type_at_use(candidate_def, ast::ExprRef::from(expression), ty, tcx);
            }
        }

        self.store_expression_type(expression, ty);
        ty
    }

    /// Applies the provided type context to an already inferred type.
    fn apply_type_context(&mut self, mut ty: Type<'db>, tcx: TypeContext<'db>) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        // Avoid promoting explicitly annotated literal values.
        if let Type::LiteralValue(literal) = ty
            && let Some(tcx) = tcx.annotation()
            && let literal_tcx @ (Type::Union(_) | Type::LiteralValue(_)) = tcx
                .resolve_type_alias(db)
                .filter_union(db, |ty| ty.as_literal_value().is_some())
            && ty.is_assignable_to(db, env, literal_tcx)
        {
            ty = Type::LiteralValue(literal.to_unpromotable());
        }

        ty
    }

    /// Specialize a bare generic class value based on type context.
    ///
    /// Currently supports only Callable type contexts.
    ///
    /// This lets `list` in a `Callable[[], list[str]]` context be treated as `list[str]`.
    fn specialize_generic_class_from_context(&self, ty: Type<'db>, target: Type<'db>) -> Type<'db> {
        let env = self.program_environment();
        // TODO: The constraint-set assignability rules should already be
        // able to determine that `list` (coerced into a callable) is assignable
        // to `Callable[[], list[str]]` when `_T@list = str`. However, when
        // comparing callables, if either is generic, we existentially quantify
        // away its typevars, transforming `∃ _T@list . _T@list = str` into
        // `always`. If we _didn't_ perform that quantification, we would have
        // the information we need to choose an appropriate specialization of
        // `list` given the type context, and we wouldn't have to duplicate all
        // of the logic below.
        let Type::ClassLiteral(class) = ty else {
            return ty;
        };
        let db = self.db();
        let exactly_one_callable = |union: UnionType<'db>| {
            union
                .elements(db)
                .iter()
                .filter_map(|element| element.resolve_type_alias(db).as_callable())
                .exactly_one()
                .ok()
        };
        let Some(target_callable) = (match target {
            Type::Callable(callable) => Some(callable),
            Type::Union(union) => exactly_one_callable(union),
            Type::TypeAlias(_) => match target.resolve_type_alias(db) {
                Type::Callable(callable) => Some(callable),
                Type::Union(union) => exactly_one_callable(union),
                _ => None,
            },
            _ => None,
        }) else {
            return ty;
        };
        // Callables made entirely of dynamic types provide no constraints for specializing the
        // class. The same is true for a parameterless context whose return type is an
        // unspecialized variable from an enclosing generic call.
        if target_callable.signatures(db).iter().all(|signature| {
            let parameters = signature.parameters().as_slice();
            (signature.return_ty.is_dynamic()
                && parameters
                    .iter()
                    .all(|parameter| parameter.annotated_type().is_dynamic()))
                || (parameters.is_empty()
                    && matches!(
                        signature.return_ty,
                        Type::Dynamic(DynamicType::UnspecializedTypeVar)
                    ))
        }) {
            return ty;
        }
        let Some(class_generic_context) = class.generic_context(db) else {
            return ty;
        };
        let Some(source_callable) = ty.try_upcast_to_callable(db, env) else {
            return ty;
        };
        // The callable relation existentially solves variables bound by each signature. Keep
        // method-local constructor variables scoped there, but expose class variables to this
        // outer contextual-specialization solve.
        let source_callable = source_callable.map(|callable| {
            let signatures = CallableSignature::from_overloads(
                callable.signatures(db).overloads.iter().map(|signature| {
                    let signature_generic_context = signature.generic_context.and_then(|context| {
                        let mut variables = context
                            .variables(db)
                            .filter(|typevar| {
                                !class_generic_context.contains(db, typevar.identity(db))
                            })
                            .peekable();
                        variables
                            .peek()
                            .is_some()
                            .then(|| GenericContext::from_typevar_instances(db, env, variables))
                    });
                    Signature::new_generic(
                        signature_generic_context,
                        signature.parameters().clone(),
                        signature.return_ty,
                    )
                    .with_definition(signature.definition())
                }),
            );
            CallableType::new(db, signatures, callable.kind(db), callable.provenance(db))
        });
        let inferable = class_generic_context.inferable_typevars(db);
        let constraints = ConstraintSetBuilder::new();
        let path_bounds = source_callable
            .into_type(db, env)
            .assignable_solutions_with_inferable(
                db,
                env,
                Type::Callable(target_callable),
                inferable,
            );
        let Solutions::Constrained(solutions) = path_bounds.solve(db, env, &constraints) else {
            return ty;
        };

        let mut type_context_mappings: FxHashMap<BoundTypeVarIdentity<'db>, UnionAccumulator<'db>> =
            FxHashMap::default();
        for solution in solutions {
            for binding in solution {
                let inferred_ty = binding
                    .solution
                    .filter_union(db, |ty| !ty.has_unspecialized_type_var(db, env));
                if inferred_ty.has_unspecialized_type_var(db, env) {
                    continue;
                }

                type_context_mappings
                    .entry(binding.bound_typevar.identity(db))
                    .and_modify(|existing| existing.add(db, env, inferred_ty))
                    .or_insert_with(|| UnionAccumulator::new(inferred_ty));
            }
        }

        if type_context_mappings.is_empty() {
            return ty;
        }

        let type_context_mappings: FxHashMap<BoundTypeVarIdentity<'db>, Type<'db>> =
            type_context_mappings
                .into_iter()
                .map(|(identity, accumulator)| (identity, accumulator.into_type(db, env)))
                .collect();
        let specialized = Type::from(class.apply_specialization(db, |generic_context| {
            generic_context.specialize_recursive(
                db,
                generic_context
                    .variables(db)
                    .map(|typevar| type_context_mappings.get(&typevar.identity(db)).copied()),
            )
        }));
        if specialized.is_assignable_to(db, env, Type::Callable(target_callable)) {
            specialized
        } else {
            ty
        }
    }

    #[track_caller]
    fn store_expression_type(&mut self, expression: &ast::Expr, ty: Type<'db>) {
        let previous = self.expressions.insert(expression.into(), ty);
        assert_eq!(previous, None);
    }

    /// Whether this region's inference should record expected types for string-literal
    /// completions. They're only ever read for files open in the editor.
    fn collects_expected_types(&self) -> bool {
        self.db().is_open_file(self.file())
    }

    fn store_maybe_expected_type(
        &mut self,
        expression: impl Into<ExpressionNodeKey>,
        ty: Type<'db>,
    ) {
        // Cheaper check first so most queries never depend on the open-file state
        if !self.has_string_literal_completion_candidates(ty) {
            return;
        }

        self.store_expected_type(expression, ty);
    }

    fn store_expected_type(&mut self, expression: impl Into<ExpressionNodeKey>, ty: Type<'db>) {
        if !self.collects_expected_types() {
            return;
        }

        self.expected_types.insert(expression.into(), ty);
    }

    fn has_string_literal_completion_candidates(&self, ty: Type<'db>) -> bool {
        match ty {
            Type::LiteralValue(literal) => literal.as_string().is_some(),
            Type::Union(union) => union
                .elements(self.db())
                .iter()
                .any(|ty| self.has_string_literal_completion_candidates(*ty)),
            Type::Intersection(intersection) => intersection
                .iter_positive(self.db())
                .any(|ty| self.has_string_literal_completion_candidates(ty)),
            Type::TypeAlias(_) => true,
            _ => false,
        }
    }

    fn union_expected_types(&mut self, expected_types: &FxHashMap<ExpressionNodeKey, Type<'db>>) {
        let db = self.db();
        let env = self.program_environment();
        // Non-empty only if the producing inference collected, i.e. the file is open
        if expected_types.is_empty() {
            return;
        }

        #[expect(
            clippy::iter_over_hash_type,
            reason = "expected types for distinct expressions are unioned independently"
        )]
        for (expression, ty) in expected_types {
            self.expected_types
                .entry(*expression)
                .and_modify(|existing| {
                    *existing = UnionType::from_two_elements(db, env, *existing, *ty);
                })
                .or_insert(*ty);
        }
    }

    fn infer_number_literal_expression(&self, literal: &ast::ExprNumberLiteral) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let ast::ExprNumberLiteral {
            range: _,
            node_index: _,
            value,
        } = literal;
        match value {
            ast::Number::Int(n) => n
                .as_i64()
                .map(Type::int_literal)
                .unwrap_or_else(|| KnownClass::Int.to_instance(db, env)),
            ast::Number::Float(v) => {
                if self.is_basedpython_file() {
                    Type::float_literal(*v)
                } else {
                    KnownClass::Float.to_instance(db, env)
                }
            }
            ast::Number::Complex { real, imag } => {
                if self.is_basedpython_file() {
                    Type::complex_literal(db, *real, *imag)
                } else {
                    KnownClass::Complex.to_instance(db, env)
                }
            }
        }
    }

    #[expect(clippy::unused_self)]
    fn infer_boolean_literal_expression(&self, literal: &ast::ExprBooleanLiteral) -> Type<'db> {
        let ast::ExprBooleanLiteral {
            range: _,
            node_index: _,
            value,
        } = literal;

        Type::bool_literal(*value)
    }

    fn infer_string_literal_expression(
        &mut self,
        literal: &ast::ExprStringLiteral,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        if let Some(expected) = tcx.annotation() {
            self.store_maybe_expected_type(ast::ExprRef::from(literal), expected);
        }

        if tcx.is_typealias() {
            let aliased_type = self.infer_string_type_expression(literal);
            return Type::KnownInstance(KnownInstanceType::LiteralStringAlias(InternedType::new(
                self.db(),
                aliased_type,
            )));
        }
        if literal.value.len() <= Self::MAX_STRING_LITERAL_SIZE {
            Type::string_literal(self.db(), literal.value.to_str())
        } else {
            Type::literal_string()
        }
    }

    fn infer_bytes_literal_expression(&mut self, literal: &ast::ExprBytesLiteral) -> Type<'db> {
        // TODO: ignoring r/R prefixes for now, should normalize bytes values
        let bytes: Vec<u8> = literal.value.bytes().collect();
        Type::bytes_literal(self.db(), &bytes)
    }

    fn infer_fstring_expression(&mut self, fstring: &ast::ExprFString) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let ast::ExprFString {
            range: _,
            node_index: _,
            value,
        } = fstring;

        // basedpython: an f-string is the pattern it spells, not `str` — the text
        // between the holes is known, and each hole is `str(x)` over the type of
        // what it interpolates. the result is promotable, so it widens to `str`
        // wherever a string literal would (an element of a mutable list, say)
        let mut template_parts: Option<Vec<TemplatePart<'db>>> =
            self.is_basedpython_file().then(Vec::new);

        let mut collector = StringPartsCollector::new();
        for part in value {
            // Make sure we iter through every parts to infer all sub-expressions. The `collector`
            // struct ensures we don't allocate unnecessary strings.
            match part {
                ast::FStringPart::Literal(literal) => {
                    if let Some(parts) = template_parts.as_mut() {
                        parts.push(TemplatePart::Text(CompactString::new(&literal.value)));
                    }
                    collector.push_str(&literal.value);
                }
                ast::FStringPart::FString(fstring) => {
                    for element in &fstring.elements {
                        match element {
                            ast::InterpolatedStringElement::Interpolation(element) => {
                                let ast::InterpolatedElement {
                                    range: _,
                                    node_index: _,
                                    expression,
                                    debug_text,
                                    conversion,
                                    format_spec,
                                } = element;
                                let ty = self.infer_expression(expression, TypeContext::default());

                                if let Some(format_spec) = format_spec {
                                    for element in format_spec.elements.interpolations() {
                                        self.infer_expression(
                                            &element.expression,
                                            TypeContext::default(),
                                        );
                                    }
                                }

                                format::check_interpolation(&self.context, element, ty);

                                // TODO: the *type* of a field with a conversion or
                                // a format spec is still just `str`; the checked
                                // `__format__` call could give back the literal
                                if debug_text.is_some()
                                    || !conversion.is_none()
                                    || format_spec.is_some()
                                {
                                    // what fills the hole is no longer `str(x)`
                                    template_parts = None;
                                    collector.add_non_literal_string_expression();
                                } else {
                                    if let Some(parts) = template_parts.as_mut() {
                                        parts.push(TemplatePart::Hole(ty));
                                    }
                                    let str_ty = ty.str(db, env);
                                    if let Some(literal) = str_ty.as_string_literal() {
                                        collector.push_str(literal.value(self.db()));
                                    } else if str_ty.is_subtype_of(db, env, Type::literal_string())
                                    {
                                        collector.add_literal_string_expression();
                                    } else {
                                        collector.add_non_literal_string_expression();
                                    }
                                }
                            }
                            ast::InterpolatedStringElement::Literal(literal) => {
                                if let Some(parts) = template_parts.as_mut() {
                                    parts.push(TemplatePart::Text(CompactString::new(
                                        &literal.value,
                                    )));
                                }
                                collector.push_str(&literal.value);
                            }
                        }
                    }
                }
            }
        }
        if let Some(parts) = template_parts {
            return TemplateLiteralType::from_parts(db, env, parts, Promotable::Yes);
        }
        collector.string_type(&self.context)
    }

    fn infer_tstring_expression(&mut self, tstring: &ast::ExprTString) -> Type<'db> {
        let db = self.db();
        let ast::ExprTString { value, .. } = tstring;
        for tstring in value {
            for element in &tstring.elements {
                match element {
                    ast::InterpolatedStringElement::Interpolation(
                        tstring_interpolation_element,
                    ) => {
                        let ast::InterpolatedElement {
                            expression,
                            format_spec,
                            ..
                        } = tstring_interpolation_element;
                        self.infer_expression(expression, TypeContext::default());
                        if let Some(format_spec) = format_spec {
                            for element in format_spec.elements.interpolations() {
                                self.infer_expression(&element.expression, TypeContext::default());
                            }
                        }
                    }
                    ast::InterpolatedStringElement::Literal(_) => {}
                }
            }
        }
        KnownClass::Template.to_instance(db, self.program_environment())
    }

    fn infer_ellipsis_literal_expression(
        &mut self,
        _literal: &ast::ExprEllipsisLiteral,
    ) -> Type<'db> {
        let db = self.db();
        KnownClass::EllipsisType.to_instance(db, self.program_environment())
    }

    /// Build a synthesized `typing.NamedTuple` class for an anonymous named
    /// tuple expression, returning the class as a `Type::ClassLiteral`.
    /// Field types come from the AST via `infer_type_expression` for the
    /// type form (`(name: T, ...)`) and from `infer_expression` (with literal
    /// promotion) for the value form (`(name=v, ...)`).
    ///
    /// Identity is shape-based: the salsa-interned `NamedTupleSpec` is
    /// derived from the field list, and the anchor uses a constant offset
    /// at module scope so two structurally identical anonymous named tuples
    /// resolve to the *same* `DynamicNamedTupleLiteral`.
    /// Lowers a basedpython parameter-shape tuple in type position to a
    /// real `tuple[...]` type when it contains variadic markers. Returns
    /// `None` if the tuple has no variadic — caller falls back to the
    /// named-tuple synthesis path so the surface syntax round-trips
    pub(super) fn lower_parameter_shape_to_tuple_type(
        &mut self,
        tuple: &ast::ExprTuple,
    ) -> Option<Type<'db>> {
        use crate::types::tuple::{Tuple, TupleType};

        // detect variadic up-front without inferring — type inference must
        // be performed exactly once per expression, so we only enter the
        // inference loop after committing to the tuple-type path
        enum Kind {
            Fixed,
            Variadic,
            /// `(*: *Args)` — the star's annotation is itself an unpack, which names the whole
            /// run of fields rather than typing each one, exactly as it does for the callable
            /// form `(*: *Args) -> None`
            UnpackedVariadic,
            KwVariadic,
        }
        fn classify(elt: &ast::Expr) -> Kind {
            let variadic = |annotation: &ast::Expr| {
                if annotation.is_starred_expr() {
                    Kind::UnpackedVariadic
                } else {
                    Kind::Variadic
                }
            };
            match elt {
                ast::Expr::Named(named) => match named.target.as_ref() {
                    ast::Expr::Starred(starred) => match starred.value.as_ref() {
                        ast::Expr::Starred(_) => Kind::KwVariadic,
                        _ => variadic(named.value.as_ref()),
                    },
                    _ => Kind::Fixed,
                },
                ast::Expr::Starred(s) => match s.value.as_ref() {
                    ast::Expr::Starred(_) => Kind::KwVariadic,
                    _ => Kind::Variadic,
                },
                _ => Kind::Fixed,
            }
        }
        let env = self.program_environment();
        let kinds: Vec<Kind> = tuple.elts.iter().map(classify).collect();
        let has_variadic = kinds
            .iter()
            .any(|k| matches!(k, Kind::Variadic | Kind::UnpackedVariadic));
        if !has_variadic {
            return None;
        }

        let db = self.db();
        let mut entries: Vec<(Kind, Type<'db>)> = Vec::with_capacity(tuple.elts.len());
        for (elt, kind) in tuple.elts.iter().zip(kinds) {
            let ty = match elt {
                ast::Expr::Named(named) => self.infer_type_expression(&named.value),
                ast::Expr::Starred(s) => match s.value.as_ref() {
                    ast::Expr::Starred(inner) => self.infer_type_expression(&inner.value),
                    _ => self.infer_type_expression(&s.value),
                },
                _ => self.infer_type_expression(elt),
            };
            entries.push((kind, ty));
        }

        // a tuple type has one variable-length segment at most, so a second variadic merges
        // into the first — `concat` unions the two rather than reporting, matching python's
        // `tuple[*Ts]` constraint without failing the annotation outright
        let mut builder = TupleSpecBuilder::with_capacity(entries.len());
        for (kind, ty) in entries {
            builder = match kind {
                Kind::Fixed => {
                    builder.push(ty);
                    builder
                }
                Kind::Variadic => builder.concat(db, env, &Tuple::homogeneous(ty)),
                Kind::UnpackedVariadic => {
                    if let Some(unpacked) = ty.exact_tuple_instance_spec(db) {
                        builder.concat(db, env, &unpacked)
                    } else if let Type::TypeVar(typevar) = ty
                        && typevar.is_typevartuple(db)
                    {
                        builder.concat_variadic_typevar(db, env, typevar)
                    } else {
                        // not something that can be spliced; the unpack itself has already
                        // reported, so fall back to typing every field with it
                        builder.concat(db, env, &Tuple::homogeneous(ty))
                    }
                }
                Kind::KwVariadic => {
                    // kwargs has no positional tuple equivalent — drop
                    builder
                }
            };
        }

        Some(Type::tuple(TupleType::new(db, env, &builder.build())))
    }

    /// basedpython: the keyword-variadic pack `expr` names, as `{**Kwargs}` does.
    ///
    /// A bare pack is not a type, so it is inferred with `ALLOW_PARAMSPEC_TYPE_EXPR` to reach the
    /// pack itself rather than the diagnostic that spelling normally earns. The probe does not
    /// store, so a `**` value that turns out to be something else is left for the caller's
    /// fallback path to infer.
    fn keyword_pack_reference(&mut self, expr: &ast::Expr) -> Option<Type<'db>> {
        if !matches!(expr, ast::Expr::Name(_)) {
            return None;
        }
        let previously_allowed_paramspec = self
            .context
            .inference_flags
            .replace(InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR, true);
        let ty = self.infer_type_expression_no_store(expr);
        let is_pack =
            matches!(ty, Type::TypeVar(typevar) if typevar.is_keyword_variadic(self.db()));
        if is_pack {
            self.infer_type_expression(expr);
        }
        self.context.inference_flags.set(
            InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR,
            previously_allowed_paramspec,
        );
        is_pack.then_some(ty)
    }

    /// basedpython: synthesize a `TypedDict` class from a `{"key": T, ...}`
    /// dict-literal type expression. Returns `None` if any key is not a
    /// string-literal expression — in that case the caller falls through
    /// to the standard "dict literal not allowed in type position"
    /// diagnostic.
    fn synthesize_typed_dict_literal(&mut self, dict: &ast::ExprDict) -> Option<Type<'db>> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        use crate::types::typed_dict::{TypedDictFieldBuilder, TypedDictSchema};
        use ty_python_core::global_scope;
        let env = self.program_environment();

        if dict.items.is_empty() {
            return None;
        }

        let db = self.db();
        let mut schema = TypedDictSchema::default();
        let mut packs: Vec<Type<'db>> = Vec::new();
        let mut hasher = DefaultHasher::new();
        for item in &dict.items {
            let Some(key_expr) = item.key.as_ref() else {
                // `**: T` extra-items marker — encoded as `Starred(Starred(T))`.
                // TODO: wire this into `TypedDictSchema.extra_items` once
                // PEP 728 support lands. for now we accept the syntax and
                // infer the type so user-visible diagnostics still fire, but
                // we don't yet enforce extra-key matching
                if let ast::Expr::Starred(outer) = &item.value
                    && let ast::Expr::Starred(inner) = outer.value.as_ref()
                {
                    let _ = self.infer_type_expression(&inner.value);
                    "**".hash(&mut hasher);
                    continue;
                }
                // basedpython `{**Kwargs}`: the pack contributes no fields until it is
                // specialized, so it is carried on the anchor and spliced in by the type mapping
                if let Some(pack) = self.keyword_pack_reference(&item.value) {
                    pack.display(db, env).to_string().hash(&mut hasher);
                    packs.push(pack);
                    continue;
                }
                return None;
            };
            let ast::Expr::StringLiteral(s) = key_expr else {
                return None;
            };
            let name = Name::new(s.value.to_str());
            let field_ty = self.infer_type_expression(&item.value);
            name.as_str().hash(&mut hasher);
            field_ty.display(db, env).to_string().hash(&mut hasher);
            schema.insert(
                name,
                TypedDictFieldBuilder::new(field_ty).required(true).build(),
            );
        }

        #[expect(clippy::cast_possible_truncation)]
        let truncated = hasher.finish() as u32;
        let class_name = Name::new(format!("_TypedDict_{truncated:08x}"));

        let module_scope = global_scope(db, db.program_file(self.file()));
        let anchor = DynamicTypedDictAnchor::Synthesized {
            scope: module_scope,
            range: dict.range(),
            schema,
            packs: packs.into_boxed_slice(),
        };
        let td = DynamicTypedDictLiteral::new(db, class_name, anchor, TypedDictModule::Typing);
        Type::ClassLiteral(ClassLiteral::DynamicTypedDict(td)).to_instance_approximation(db, env)
    }

    /// basedpython: synthesize the protocol type an inline `protocol(...)` type expression
    /// denotes.
    ///
    /// The result is a [structural protocol](Type::inline_protocol) rather than a class, so two
    /// inline protocols with the same members are the same type wherever they are written. A
    /// duplicate member name is rejected by the parser, so the last binding for a name wins here.
    fn synthesize_inline_protocol(&mut self, protocol: &ast::ExprProtocolType) -> Type<'db> {
        use crate::types::protocol_class::InlineProtocolMember;
        let env = self.program_environment();

        let mut members: Vec<(Name, InlineProtocolMember<'db>)> =
            Vec::with_capacity(protocol.members.len());
        let mut packs: Vec<Type<'db>> = Vec::new();

        for member in &protocol.members {
            match member {
                // `**Kwargs` — a keyword-variadic pack, spliced in field by field once the pack
                // is specialized. Parsed as `Starred(Starred(_))`, as everywhere `**` unpacks
                ast::Expr::Starred(outer) => {
                    let inner = match outer.value.as_ref() {
                        ast::Expr::Starred(inner) => inner.value.as_ref(),
                        other => other,
                    };
                    if let Some(pack) = self.keyword_pack_reference(inner) {
                        packs.push(pack);
                        continue;
                    }
                    let ty = self.infer_type_expression(inner);
                    if let Some(builder) = self
                        .context
                        .report_lint(&INVALID_TYPE_FORM, AnyNodeRef::from(member))
                    {
                        builder.into_diagnostic(format_args!(
                            "Only a keyword-variadic pack can be unpacked into an inline \
                             protocol, not `{}`",
                            ty.display(self.db(), env)
                        ));
                    }
                }
                // `def f(self) -> int` — a method member, whose receiver binds on access
                ast::Expr::ProtocolMethod(method) => {
                    let ty = self.infer_protocol_method_signature(method);
                    let member = match ty {
                        Type::Callable(callable) => InlineProtocolMember::Method(callable),
                        // the signature is always a callable arrow, so this only happens when its
                        // own inference already failed and reported
                        _ => InlineProtocolMember::Attribute(ty),
                    };
                    members.push((method.name.id.clone(), member));
                }
                // `a: int` — a data member, mutable as in a protocol class body
                ast::Expr::Named(named) => {
                    let ty = self.infer_type_expression(&named.value);
                    if let Some(name) = named.target.as_name_expr() {
                        members.push((name.id.clone(), InlineProtocolMember::Attribute(ty)));
                    }
                }
                other => {
                    let ty = self.infer_type_expression(other);
                    if let Some(builder) = self
                        .context
                        .report_lint(&INVALID_TYPE_FORM, AnyNodeRef::from(other))
                    {
                        builder.into_diagnostic(format_args!(
                            "`{}` is not a valid inline protocol member; expected `name: T`, \
                             `def name(...) -> T`, or `**Pack`",
                            ty.display(self.db(), env)
                        ));
                    }
                }
            }
        }

        Type::inline_protocol(self.db(), env, members, packs.into_boxed_slice())
    }

    fn synthesize_anon_named_tuple_class(
        &mut self,
        tuple: &ast::ExprTuple,
        is_type_form: bool,
    ) -> Type<'db> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        use ty_python_core::global_scope;
        let env = self.program_environment();

        let db = self.db();
        let mut fields: Vec<NamedTupleField<'db>> = Vec::with_capacity(tuple.elts.len());
        for (i, elt) in tuple.elts.iter().enumerate() {
            let (field_name, ty) = match elt {
                ast::Expr::Named(named) => {
                    let name_str = named
                        .target
                        .as_name_expr()
                        .map(|n| n.id.as_str().to_owned())
                        .unwrap_or_else(|| format!("arg{i}"));
                    let ty = if is_type_form {
                        self.infer_type_expression(&named.value)
                    } else {
                        // value form preserves literal types so a tuple
                        // literal like `(1, name="a")` reveals
                        // `(1, name="a")` not `(int, name=str)`
                        self.infer_expression(&named.value, TypeContext::default())
                    };
                    (name_str, ty)
                }
                other => {
                    let ty = if is_type_form {
                        self.infer_type_expression(other)
                    } else {
                        self.infer_expression(other, TypeContext::default())
                    };
                    (format!("arg{i}"), ty)
                }
            };
            fields.push(NamedTupleField {
                name: ruff_python_ast::name::Name::new(&field_name),
                ty,
                default: None,
                definition: None,
            });
        }

        let spec = NamedTupleSpec::known(db, fields.into_boxed_slice());

        // Shape-based identity: hash the field-name + display-string of the
        // field type to derive the synthesized class name. Two anonymous
        // named tuples with the same shape produce the same name and the
        // same anchor (which itself is keyed off the salsa-interned spec
        // plus a constant scope+offset), so they resolve to the same
        // `DynamicNamedTupleLiteral`.
        let mut hasher = DefaultHasher::new();
        for field in spec.fields(db) {
            field.name.as_str().hash(&mut hasher);
            field.ty.display(db, env).to_string().hash(&mut hasher);
        }
        #[expect(clippy::cast_possible_truncation)]
        let truncated = hasher.finish() as u32;
        let class_name =
            ruff_python_ast::name::Name::new(format!("_AnonNamedTuple_{truncated:08x}"));

        // Anchor at the module-scope with a constant offset of 0 so structural
        // identity (driven entirely by `spec`) determines the synthesized
        // class. Different shapes produce different specs; identical shapes
        // unify across the file.
        let module_scope = global_scope(db, db.program_file(self.file()));
        let anchor = DynamicNamedTupleAnchor::ScopeOffset {
            scope: module_scope,
            offset: 0,
            spec,
        };

        let nt = DynamicNamedTupleLiteral::new(db, class_name, anchor);
        Type::ClassLiteral(ClassLiteral::DynamicNamedTuple(nt))
    }

    fn infer_tuple_expression(
        &mut self,
        tuple: &ast::ExprTuple,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        /// If a tuple literal has more elements than this constant,
        /// we promote `Literal` types when inferring the elements of the tuple.
        /// This provides a huge speedup on files that have very large unannotated tuple literals.
        const MAX_TUPLE_LENGTH_FOR_UNANNOTATED_LITERAL_INFERENCE: usize = 64;
        let env = self.program_environment();
        let db = self.db();

        // basedpython anonymous named tuple type literal in value position.
        // E.g. `a = (name: str, age: int)` is a type-alias-like expression
        // whose value is the class object for the synthesized `NamedTuple`.
        if tuple.is_anon_named_tuple {
            let class_lit =
                self.synthesize_anon_named_tuple_class(tuple, /* is_type_form = */ true);
            return class_lit;
        }
        // basedpython anonymous named tuple value literal: `(name=v, ...)`.
        // Each field's value type is inferred from the value expression; the
        // result type is an *instance* of the synthesized `NamedTuple` class
        // so attribute access (`a.name`) resolves the correct field type.
        if tuple.is_anon_named_tuple_value {
            let class_lit =
                self.synthesize_anon_named_tuple_class(tuple, /* is_type_form = */ false);
            let literal_instance = class_lit
                .to_instance_approximation(self.db(), env)
                .unwrap_or(class_lit);
            // an expected anonymous-named-tuple shape wins over the literal one, the
            // same coercion the plain-tuple spelling gets below: `b: P = (name="a")`
            // and `b: P = ("a",)` construct the one class the transpiler emits, whose
            // fields carry the *declared* types — so `b._replace(name="x")` is a call
            // the runtime accepts
            if let Some(target) = tcx
                .annotation()
                .and_then(|annotation| find_anon_nt_target(self.db(), env, annotation))
                && literal_instance.is_assignable_to(self.db(), env, target)
            {
                return target;
            }
            return literal_instance;
        }
        // basedpython parameter-shape tuple literal in value position is not
        // a runtime type itself — infer each contained field for diagnostics
        // and return `Unknown`. (Type-position handling is in
        // `type_expression.rs`.)
        if tuple.has_parameter_shape() {
            for elt in &tuple.elts {
                if let ast::Expr::Named(named) = elt {
                    let _ = self.infer_type_expression(&named.value);
                } else {
                    let _ = self.infer_type_expression(elt);
                }
            }
            return Type::unknown();
        }
        // basedpython: a *plain* tuple literal in a position whose expected
        // type (via bidirectional inference) is an anonymous-named-tuple
        // instance gets inferred AS that instance — matching the transpiler's
        // implicit constructor coercion. This makes `def f() -> (name: str,
        // age: int): return ("a", 1)` type-check, since at runtime the
        // returned value is wrapped as `_AnonNamedTuple_xxx("a", 1)`.
        // Find an anon-NT instance candidate in the expected type. Accept the
        // direct case (`x: anon-NT`) and any union member (`x: anon-NT | None`,
        // `x: anon-NT | Other`, …). The first matching union member wins. A
        // `type P = (name: str)` alias is resolved through, so the coercion
        // follows what the annotation means rather than how it is spelled —
        // which is also what the transpiler wraps on.
        #[expect(clippy::items_after_statements, reason = "helper colocated with use")]
        fn find_anon_nt_target<'db>(
            db: &'db dyn crate::Db,
            env: &ProgramEnvironment<'db>,
            ty: Type<'db>,
        ) -> Option<Type<'db>> {
            let ty = ty.resolve_type_alias(db);
            if let Type::NominalInstance(instance) = ty
                && let crate::types::class::ClassLiteral::DynamicNamedTuple(nt) =
                    instance.class(db, env).class_literal(db)
                && nt.name(db).as_str().starts_with("_AnonNamedTuple_")
            {
                return Some(ty);
            }
            if let Type::Union(union) = ty {
                for member in union.elements(db).iter().copied() {
                    if let Some(found) = find_anon_nt_target(db, env, member) {
                        return Some(found);
                    }
                }
            }
            None
        }
        if let Some(target_instance) = tcx
            .annotation()
            .and_then(|a| find_anon_nt_target(self.db(), env, a))
            && let Type::NominalInstance(instance) = target_instance
            && let crate::types::class::ClassLiteral::DynamicNamedTuple(nt) =
                instance.class(self.db(), env).class_literal(self.db())
            && nt.name(self.db()).as_str().starts_with("_AnonNamedTuple_")
        {
            let spec = match nt.anchor(self.db()) {
                crate::types::class::DynamicNamedTupleAnchor::ScopeOffset { spec, .. } => {
                    Some(*spec)
                }
                _ => None,
            };
            if let Some(spec) = spec {
                let fields = spec.fields(self.db());
                if fields.len() == tuple.elts.len()
                    && tuple
                        .elts
                        .iter()
                        .all(|e| !matches!(e, ast::Expr::Starred(_)))
                {
                    // type-check element-wise against the expected field
                    // types. when every element is assignable, return the
                    // anon-NT instance so attribute access works; otherwise
                    // build a plain heterogeneous tuple from the inferred
                    // types (each element has already been inferred, so we
                    // can't re-call `infer_expression` in a fall-through
                    // branch)
                    let mut elt_tys: Vec<Type<'db>> = Vec::with_capacity(tuple.elts.len());
                    let mut all_assignable = true;
                    for (elt, field) in tuple.elts.iter().zip(fields.iter()) {
                        let elt_ty = self.infer_expression(elt, TypeContext::new(Some(field.ty)));
                        if !elt_ty.is_assignable_to(self.db(), env, field.ty) {
                            all_assignable = false;
                        }
                        elt_tys.push(elt_ty);
                    }
                    if all_assignable {
                        return target_instance;
                    }
                    return Type::heterogeneous_tuple(self.db(), env, elt_tys);
                }
            }
        }

        let env = self.program_environment();
        let ast::ExprTuple {
            range: _,
            node_index: _,
            elts,
            ctx: _,
            parenthesized: _,
            is_anon_named_tuple: _,
            is_anon_named_tuple_value: _,
            callable_shape: _,
            is_parameter_shape: _,
        } = tuple;

        // Remove any union elements of the annotation that are unrelated to the tuple type.
        let tcx = tcx.map(|annotation| {
            let inferable = KnownClass::Tuple
                .try_to_class_literal(db, env)
                .and_then(|class| class.generic_context(db))
                .map(|generic_context| generic_context.inferable_typevars(db))
                .unwrap_or(TypeVarSet::None);
            annotation.filter_disjoint_elements(
                db,
                env,
                Type::homogeneous_tuple(db, env, Type::unknown()),
                inferable,
            )
        });

        let mut is_homogeneous_tuple_annotation = false;

        let annotated_tuple = tcx
            .known_specialization(db, env, KnownClass::Tuple)
            .and_then(|specialization| {
                let spec = specialization
                    .tuple(self.db())
                    .expect("the specialization of `KnownClass::Tuple` must have a tuple spec");

                if let Tuple::Variable(tuple) = spec
                    && tuple.prefix_elements().is_empty()
                    && tuple.suffix_elements().is_empty()
                    && matches!(tuple.variable(), VariableSegment::Homogeneous(_))
                {
                    is_homogeneous_tuple_annotation = true;
                }

                spec.resize(db, env, TupleLength::Fixed(elts.len())).ok()
            });

        // TODO: this is a simplification for now.
        //
        // It might be possible to use the type context where the annotation is not a pure-homogeneous
        // tuple and the actual tuple has starred elements in it. It seems complex to reason about,
        // though, and unlikely to come up much.
        let can_use_type_context =
            is_homogeneous_tuple_annotation || elts.iter().all(|elt| !elt.is_starred_expr());

        let annotated_elt_tys = annotated_tuple
            .as_ref()
            .map(|tuple| tuple.iter_element_types(self.db()).collect::<Vec<_>>())
            .unwrap_or_default();
        let mut annotated_elt_tys = annotated_elt_tys.into_iter();

        let mut infer_element = |elt: &ast::Expr| {
            let annotated_elt_ty = annotated_elt_tys.by_ref().next();
            let element_tcx = if can_use_type_context {
                let expected = if elt.is_starred_expr() {
                    let expected_element = annotated_elt_ty.unwrap_or_else(Type::object);
                    Some(KnownClass::Iterable.to_specialized_instance(db, env, &[expected_element]))
                } else {
                    annotated_elt_ty
                };
                TypeContext::new(expected)
            } else {
                TypeContext::default()
            };
            if tuple.len() > MAX_TUPLE_LENGTH_FOR_UNANNOTATED_LITERAL_INFERENCE {
                // Promote literals for very large unannotated tuples,
                // to avoid pathological performance issues
                self.infer_expression(elt, element_tcx).promote(db, env)
            } else {
                self.infer_expression(elt, element_tcx)
            }
        };

        let mut builder = TupleSpecBuilder::with_capacity(elts.len());

        for element in elts {
            if let ast::Expr::Starred(starred) = element {
                let element_type = infer_element(element);
                // Fine to use `iterate` rather than `try_iterate` here:
                // errors from iterating over something not iterable will have been
                // emitted in the `infer_element` call above.
                let mut spec = element_type.iterate(db, env).into_owned();

                let known_length = match &*starred.value {
                    ast::Expr::List(ast::ExprList { elts, .. })
                    | ast::Expr::Set(ast::ExprSet { elts, .. }) => elts
                        .iter()
                        .all(|elt| !elt.is_starred_expr())
                        .then_some(elts.len()),
                    ast::Expr::Dict(ast::ExprDict { items, .. }) => items
                        .iter()
                        .all(|item| item.key.is_some())
                        .then_some(items.len()),
                    _ => None,
                };

                if let Some(known_length) = known_length {
                    spec = spec
                        .resize(db, env, TupleLength::Fixed(known_length))
                        .unwrap_or(spec);
                }

                builder = builder.concat(db, env, &spec);
            } else {
                builder.push(infer_element(element));
            }
        }

        Type::tuple(TupleType::new(db, env, &builder.build()))
    }

    fn infer_list_expression(&mut self, list: &ast::ExprList, tcx: TypeContext<'db>) -> Type<'db> {
        let db = self.db();
        let ast::ExprList {
            range: _,
            node_index: _,
            elts,
            ctx: _,
        } = list;

        let elts = elts.iter().map(|elt| [Some(elt)]).collect_vec();
        let mut infer_elt_ty =
            |builder: &mut Self, (_, elt, tcx)| builder.infer_expression(elt, tcx);

        self.infer_collection_literal(
            KnownClass::List,
            Some(list.into()),
            &elts,
            &mut infer_elt_ty,
            tcx,
        )
        .unwrap_or_else(|| {
            KnownClass::List.to_specialized_instance(
                db,
                self.program_environment(),
                &[Type::unknown()],
            )
        })
    }

    fn infer_set_expression(&mut self, set: &ast::ExprSet, tcx: TypeContext<'db>) -> Type<'db> {
        let db = self.db();
        let ast::ExprSet {
            range: _,
            node_index: _,
            elts,
        } = set;

        let elts = elts.iter().map(|elt| [Some(elt)]).collect_vec();
        let fallback_tcx = self.incomplete_typed_dict_key_context(set, tcx);
        let mut infer_elt_ty = |builder: &mut Self, arg: ArgExpr<'db, '_>| {
            let (_, elt, elt_tcx) = arg;
            builder.infer_set_element(elt, elt_tcx, fallback_tcx)
        };

        self.infer_collection_literal(
            KnownClass::Set,
            Some(set.into()),
            &elts,
            &mut infer_elt_ty,
            tcx,
        )
        .unwrap_or_else(|| {
            KnownClass::Set.to_specialized_instance(
                db,
                self.program_environment(),
                &[Type::unknown()],
            )
        })
    }

    /// Infers a set element, optionally with a fallback context for an incomplete `TypedDict` key.
    ///
    /// When normal set element context is available, semantic inference keeps that context. If a
    /// `TypedDict` key fallback is also available, it is only added to the stored expected type used
    /// by IDE string-literal completions.
    fn infer_set_element(
        &mut self,
        elt: &ast::Expr,
        elt_tcx: TypeContext<'db>,
        fallback_tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let db = self.db();
        let inference_tcx = if elt_tcx.annotation().is_some() {
            elt_tcx
        } else {
            fallback_tcx
        };
        let inferred_ty = self.infer_expression(elt, inference_tcx);

        // `expected_types` is IDE completion metadata. If normal set inference already has a
        // string-literal context, preserve that semantic context for inference while also offering
        // the transient `TypedDict` key fallback as a completion candidate.
        if let (Some(elt_ty), Some(fallback_ty)) = (elt_tcx.annotation(), fallback_tcx.annotation())
        {
            self.store_expected_type(
                elt,
                UnionType::from_two_elements(db, self.program_environment(), elt_ty, fallback_ty),
            );
        }

        inferred_ty
    }

    /// Returns a fallback type context for completing a `TypedDict` key while editing.
    ///
    /// While editing `{"key": value}` as a `TypedDict` literal, `{"key"}` parses as a set
    /// until the colon is typed. This preserves key completions in that transient state.
    fn incomplete_typed_dict_key_context(
        &self,
        set: &ast::ExprSet,
        tcx: TypeContext<'db>,
    ) -> TypeContext<'db> {
        let [elt] = set.elts.as_slice() else {
            return TypeContext::default();
        };

        if !elt.is_string_literal_expr() {
            return TypeContext::default();
        }

        TypeContext::new(
            tcx.annotation()
                .and_then(|annotation| self.typed_dict_key_expected_type(annotation)),
        )
    }

    fn infer_dict_expression(&mut self, dict: &ast::ExprDict, tcx: TypeContext<'db>) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let ast::ExprDict {
            range: _,
            node_index: _,
            items,
        } = dict;

        let mut item_types = FxHashMap::default();

        // Validate `TypedDict` dictionary literal assignments.
        if let Some(annotation) = tcx
            .annotation()
            .map(|annotation| annotation.resolve_type_alias(self.db()))
        {
            if let Some(typed_dict) = annotation.as_typed_dict() {
                // If there is a single typed dict annotation, infer against it directly.
                if let Some(ty) =
                    self.infer_typed_dict_expression(dict, typed_dict, &mut item_types)
                {
                    return ty;
                }
            } else if let Type::Union(union) = annotation {
                let union_elements = union.elements(self.db());
                let mut typed_dicts = Vec::new();
                let mut has_dict_compatible_fallback = false;

                for element in union_elements {
                    let element = element.resolve_type_alias(db);

                    if let Some(typed_dict) = element.as_typed_dict() {
                        typed_dicts.push(typed_dict);
                    } else if !has_dict_compatible_fallback {
                        // Suppress `TypedDict` diagnostics only if this literal is assignable to
                        // the non-`TypedDict` arm of the union.
                        let mut speculative_builder = self.speculate_without_diagnostics();
                        has_dict_compatible_fallback = speculative_builder
                            .infer_dict_expression(dict, TypeContext::new(Some(element)))
                            .is_assignable_to(db, env, element);
                    }
                }

                if let [typed_dict] = typed_dicts.as_slice()
                    && !has_dict_compatible_fallback
                {
                    if let Some(ty) =
                        self.infer_typed_dict_expression(dict, *typed_dict, &mut item_types)
                    {
                        return ty;
                    }
                } else if !typed_dicts.is_empty() {
                    // Infer all expressions with diagnostics enabled before starting
                    // multi-inference. This preserves the general expression types even if we later
                    // fall back to a non-`TypedDict` arm of the union.
                    for item in items {
                        if let Some(key) = item.key.as_ref() {
                            let key_ty = self.infer_expression(key, TypeContext::default());
                            item_types.insert(key.node_index().load(), key_ty);
                        }

                        let value_ty = self.infer_expression(&item.value, TypeContext::default());
                        item_types.insert(item.value.node_index().load(), value_ty);
                    }

                    let mut narrowed_tys = Vec::new();
                    let mut item_types = FxHashMap::default();
                    // Reuse nested expressions that receive the same field context across candidates.
                    let teardown_expression_cache = self.setup_expression_cache();
                    for typed_dict in typed_dicts {
                        // Suppress diagnostics for discarded candidates. A mixed union like
                        // `TypedDict | dict[str, Any]` should remain quiet when the dict arm accepts
                        // the literal.
                        if let Some(inferred_ty) = self
                            .speculate_without_diagnostics()
                            .infer_typed_dict_expression(dict, typed_dict, &mut item_types)
                        {
                            narrowed_tys.push(inferred_ty);
                        }

                        item_types.clear();
                    }
                    if teardown_expression_cache {
                        self.teardown_expression_cache();
                    }

                    // Successfully narrowed to a subset of typed dicts.
                    if !narrowed_tys.is_empty() {
                        return UnionType::from_elements(db, env, narrowed_tys);
                    }
                }
            }
        }

        let items = items
            .iter()
            .map(|item| [item.key.as_ref(), Some(&item.value)])
            .collect_vec();

        // Avoid inferring the items multiple times if we already attempted to infer the
        // dictionary literal as a `TypedDict`. This also allows us to infer using the
        // type context of the expected `TypedDict` field.
        let mut infer_elt_ty = |builder: &mut Self, (_, elt, tcx): ArgExpr<'db, '_>| {
            item_types
                .get(&elt.node_index().load())
                .copied()
                .or_else(|| builder.try_expression_type(elt))
                .unwrap_or_else(|| builder.infer_expression(elt, tcx))
        };

        self.infer_collection_literal(
            KnownClass::Dict,
            Some(dict.into()),
            &items,
            &mut infer_elt_ty,
            tcx,
        )
        .unwrap_or_else(|| {
            KnownClass::Dict.to_specialized_instance(db, env, &[Type::unknown(), Type::unknown()])
        })
    }

    // Infer the type of a collection literal expression.
    fn infer_collection_literal<'expr, const N: usize>(
        &mut self,
        collection_class: KnownClass,
        collection_expr: Option<ast::ExprRef<'_>>,
        elts: &[[Option<&'expr ast::Expr>; N]],
        infer_elt_expression: &mut dyn FnMut(&mut Self, ArgExpr<'db, 'expr>) -> Type<'db>,
        tcx: TypeContext<'db>,
    ) -> Option<Type<'db>> {
        let db = self.db();
        let env = self.program_environment();
        let mut try_narrow = |narrowed_ty| {
            let mut speculative_builder = self.speculate();

            // Attempt to infer the collection literal using the narrowed type context.
            let inferred_ty = speculative_builder.infer_collection_literal_impl(
                collection_class,
                collection_expr,
                elts,
                infer_elt_expression,
                TypeContext::new(Some(narrowed_ty)),
            )?;

            // Ensure the inferred return type is assignable to the narrowed declared type.
            if !inferred_ty.is_assignable_to(db, env, narrowed_ty) {
                return None;
            }

            // Successfully narrowed to an element of the union.
            self.extend(speculative_builder);
            Some(inferred_ty)
        };

        // If the type context is a union, attempt to narrow to a specific element.
        for narrowed_ty in tcx
            .narrow_targets(db, env)
            .as_deref()
            .into_iter()
            .flatten()
            .filter(|ty| ty.class_specialization(db, env).is_some())
        {
            if let Some(result) = try_narrow(*narrowed_ty) {
                return Some(result);
            }
        }

        self.infer_collection_literal_impl(
            collection_class,
            collection_expr,
            elts,
            infer_elt_expression,
            tcx,
        )
    }

    // Infer the type of a collection literal expression.
    fn infer_collection_literal_impl<'expr, const N: usize>(
        &mut self,
        collection_class: KnownClass,
        collection_expr: Option<ast::ExprRef<'_>>,
        elts: &[[Option<&'expr ast::Expr>; N]],
        infer_elt_expression: &mut dyn FnMut(&mut Self, ArgExpr<'db, 'expr>) -> Type<'db>,
        tcx: TypeContext<'db>,
    ) -> Option<Type<'db>> {
        let db = self.db();
        let env = self.program_environment();

        // Extract the type variable `T` from `list[T]` in typeshed.
        let elt_tys = |collection_class: KnownClass| {
            let collection_alias = collection_class
                .try_to_class_literal(db, env)?
                .identity_specialization(db)
                .into_generic_alias()?;

            let generic_context = collection_alias
                .specialization(self.db())
                .generic_context(self.db());

            Some((
                collection_alias,
                generic_context,
                generic_context.variables(self.db()),
            ))
        };

        let Some((collection_alias, generic_context, elt_tys)) = elt_tys(collection_class) else {
            // Infer the element types without type context, and fallback to `Unknown` for
            // custom typesheds.
            for (i, elt) in elts.iter().flatten().flatten().enumerate() {
                infer_elt_expression(self, (i, elt, TypeContext::default()));
            }

            return None;
        };

        let constraints = ConstraintSetBuilder::new();
        let inferable = generic_context.inferable_typevars(db);
        let identity_instance = Type::instance(db, env, ClassType::Generic(collection_alias));
        let mut builder = SpecializationBuilder::new(db, env, &constraints, inferable);

        // Remove any union elements of that are unrelated to the collection type.
        //
        // For example, we only want the `list[int]` from `annotation: list[int] | None` if
        // `collection_ty` is `list`.
        let tcx = tcx.map(|annotation| {
            let collection_ty = collection_class.to_instance(db, env);
            annotation.filter_disjoint_elements(db, env, collection_ty, inferable)
        });

        // Collect type constraints from the declared element types.
        //
        // We use a forward assignability check (`identity_instance ≤ tcx`) to infer what each
        // typevar maps to in the type context. For example, if the type context is `list[int]` and
        // `collection_instance` is `list[T]`, the check produces `T = int`.
        let (elt_tcx_constraints, elt_tcx_variance) = {
            let mut elt_tcx_constraints: FxHashMap<
                BoundTypeVarIdentity<'db>,
                UnionAccumulator<'db>,
            > = FxHashMap::default();
            let mut elt_tcx_variance: FxHashMap<BoundTypeVarIdentity<'_>, TypeVarVariance> =
                FxHashMap::default();

            if let Some(tcx) = tcx
                .annotation()
                .map(|tcx| tcx.resolve_type_alias(self.db()))
                && matches!(tcx, Type::NominalInstance(_))
                && let Some(specialization) = tcx.known_specialization(db, env, collection_class)
                && specialization.generic_context(self.db()) == generic_context
                && generic_context.variables(self.db()).all(|typevar| {
                    !typevar.is_paramspec(self.db())
                        && typevar
                            .typevar(self.db())
                            .bound_or_constraints(db, env)
                            .is_none()
                })
            {
                // For an instance of the collection class itself, the identity specialization
                // maps directly to the contextual specialization. Avoid constructing and solving
                // a general assignability constraint set for this common case.
                for (typevar, inferred_ty) in generic_context
                    .variables(self.db())
                    .zip(specialization.types(self.db()))
                {
                    let inferred_ty = inferred_ty
                        .filter_union(db, |ty| {
                            !ty.as_typevar()
                                .is_some_and(|tv| tv.is_inferable(self.db(), inferable))
                        })
                        .filter_union(db, |ty| !ty.has_unspecialized_type_var(db, env));
                    if inferred_ty.has_unspecialized_type_var(db, env) {
                        continue;
                    }

                    let identity = typevar.identity(self.db());
                    elt_tcx_constraints.insert(identity, UnionAccumulator::new(inferred_ty));
                    elt_tcx_variance.insert(identity, typevar.variance(db));
                }
            } else if let Some(tcx) = tcx.annotation()
                && tcx.class_specialization(self.db(), env).is_some()
            {
                let db = self.db();

                let path_bounds =
                    identity_instance.assignable_solutions_with_inferable(db, env, tcx, inferable);
                let solutions = path_bounds.solve_with(|variance, path_bound| {
                    let identity = path_bound.bound_typevar.identity(db);
                    elt_tcx_variance
                        .entry(identity)
                        .and_modify(|current| *current = current.join(variance))
                        .or_insert(variance);
                    PathBounds::default_solve(db, env, &constraints, path_bound)
                });

                match solutions {
                    // If the type context is not compatible with the collection type (e.g., a
                    // `list` literal where a `tuple` is expected), the assignability check
                    // produces an unsatisfiable result. In that case, we simply proceed without
                    // type context constraints rather than aborting the entire collection literal
                    // inference.
                    Solutions::Unsatisfiable | Solutions::Unconstrained => {}
                    Solutions::Constrained(solutions) => {
                        for solution in &solutions {
                            for binding in solution {
                                // The SequentMap's transitivity reasoning can inject
                                // cross-typevar references into the solution bounds.
                                // For example, `_KT ≤ str ∧ str ≤ _VT` derives `_KT ≤ _VT`,
                                // which adds `_KT` to `_VT`'s lower bound. Remove inferable
                                // typevars from the same generic context, since they represent
                                // cross-typevar relationships that are resolved independently.
                                let inferred_ty = builder
                                    .remove_inferable_typevar_artifacts_from_solution(
                                        binding.bound_typevar,
                                        binding.solution,
                                    );

                                // Avoid inferring a preferred type based on partially specialized
                                // type context from an outer generic call. If the type context is
                                // a union, we try to keep any concrete elements.
                                let inferred_ty = inferred_ty
                                    .filter_union(db, |ty| !ty.has_unspecialized_type_var(db, env));
                                if inferred_ty.has_unspecialized_type_var(db, env) {
                                    continue;
                                }

                                let identity = binding.bound_typevar.identity(db);
                                elt_tcx_constraints
                                    .entry(identity)
                                    .and_modify(|existing| {
                                        existing.add(db, env, inferred_ty);
                                    })
                                    .or_insert_with(|| UnionAccumulator::new(inferred_ty));
                            }
                        }

                        // Remove variance entries for typevars whose solutions were filtered out
                        // (e.g., due to unspecialized typevars). Variance should only be tracked
                        // for typevars with actual type context constraints.
                        elt_tcx_variance
                            .retain(|identity, _| elt_tcx_constraints.contains_key(identity));
                    }
                }
            }

            let elt_tcx_constraints: FxHashMap<BoundTypeVarIdentity<'db>, Type<'db>> =
                elt_tcx_constraints
                    .into_iter()
                    .map(|(identity, accumulator)| (identity, accumulator.into_type(db, env)))
                    .collect();

            (elt_tcx_constraints, elt_tcx_variance)
        };

        // Dictionary unpacking always contributes constraints on the inferred key and value types,
        // even when the unpacked mapping is assignable to the context. Keep it on the general path
        // so gradual types such as `Any` are preserved.
        let has_dict_unpack = collection_class == KnownClass::Dict
            && elts
                .iter()
                .any(|elts| matches!(elts.as_slice(), [None, Some(_)]));

        let mut pre_inferred_elt_tys = None;

        // Avoid projecting and solving a constraint set when contextual inference has already
        // provided the complete specialization and every literal element is compatible with it.
        if !has_dict_unpack
            && tcx.annotation().is_some()
            && let Some(specialization) = generic_context
                .variables(self.db())
                .map(|typevar| {
                    let identity = typevar.identity(self.db());
                    // Keep this parallel with the slow path below: a covariant context provides
                    // only an upper bound, which does not determine the specialization for an empty
                    // literal. A contravariant context provides a lower bound, for which inference
                    // selects the narrowest valid solution.
                    if elt_tcx_variance
                        .get(&identity)
                        .is_some_and(|variance| variance.is_covariant())
                    {
                        return None;
                    }
                    elt_tcx_constraints.get(&identity).copied()
                })
                .collect::<Option<Vec<_>>>()
        {
            // The slow path below adds the contextual specialization as an invariant mapping,
            // then discards every element constraint that is already assignable to its context.
            // Infer the elements once here and retain their types so that a failed fast-path check
            // does not recursively re-infer nested collection literals on the slow path.
            let mut inferred_elts = Vec::with_capacity(elts.len());
            let mut compatible = true;

            for elts in elts {
                let mut inferred_elt_tys = [None; N];
                for (i, elt, elt_tcx) in itertools::izip!(0.., elts, specialization.iter().copied())
                {
                    let Some(elt) = elt else { continue };
                    let elt_tcx = if elt.is_starred_expr() && collection_class != KnownClass::Dict {
                        Type::homogeneous_tuple(db, env, elt_tcx)
                    } else {
                        elt_tcx
                    };
                    let inferred_elt_ty =
                        infer_elt_expression(self, (i, elt, TypeContext::new(Some(elt_tcx))));
                    inferred_elt_tys[i] = Some(inferred_elt_ty);

                    if !inferred_elt_ty.is_assignable_to(db, env, elt_tcx) {
                        compatible = false;
                    }
                }
                inferred_elts.push(inferred_elt_tys);
            }

            if compatible {
                let class_type = collection_alias.origin(self.db()).apply_specialization(
                    db,
                    |generic_context| {
                        generic_context
                            .specialize_recursive(db, specialization.into_iter().map(Some))
                    },
                );
                return Type::from(class_type).to_instance_approximation(db, env);
            }

            pre_inferred_elt_tys = Some(inferred_elts);
        }

        // Create a set of constraints to infer a precise type for `T`.
        let mut tuple_size_promotion_constraints = TupleSizePromotionConstraints::default();

        for elt_ty in elt_tys.clone() {
            let elt_ty_identity = elt_ty.identity(self.db());
            let elt_tcx = elt_tcx_constraints
                // The annotated type acts as a constraint for `T`.
                //
                // Note that we infer the annotated type _before_ the elements, to more closely match
                // the order of any unions as written in the type annotation.
                .get(&elt_ty_identity)
                .copied();

            if elt_tcx.is_some_and(|elt_tcx| !elt_tcx.is_dynamic()) {
                // Record type annotations that provide concrete shape information in order to
                // disqualify this typevar from tuple size promotion.
                tuple_size_promotion_constraints.record_declared_type(elt_ty_identity);
            }

            // Avoid unnecessarily widening the return type based on a covariant
            // type parameter from the type context.
            //
            // Note that we also avoid unioning  the inferred type with `Unknown` in this
            // case, which is only necessary for invariant collections.
            //
            // An *empty* literal is the exception: there are no elements to infer from, so
            // the covariant bound is the only information available, and using it beats
            // falling back to `Unknown`. `v: Sequence[int] = []` solves as `list[int]` —
            // the widest `T` with `list[T]` assignable to `Sequence[int]` — which is both
            // well-defined and more precise than the gradual `list[Unknown]`.
            if !elts.is_empty()
                && elt_tcx_variance
                    .get(&elt_ty_identity)
                    .is_some_and(|variance| variance.is_covariant())
            {
                continue;
            }

            // If there is no applicable context for this element type variable, we infer from the
            // literal elements directly. This violates the gradual guarantee (we don't know that
            // our inference is compatible with subsequent additions to the collection), but it
            // matches the behavior of other type checkers and is usually the desired behavior.
            if let Some(elt_tcx) = elt_tcx {
                builder.add_type_mapping(elt_ty, elt_tcx, TypeVarVariance::Invariant);
            }
        }

        // If this collection literal is the assigned value of a fluid specialization
        // candidate, its specialization is solved in two steps: the creation-time
        // solution below retains literal types, and constraints from later uses of the
        // binding are combined with it in `fluid_eventual_type`.
        let fluid_def = if tcx.annotation().is_none() {
            collection_expr.and_then(|expr| self.fluid_candidate_definition(expr))
        } else {
            None
        };

        // basedpython: under `sound-types` an *empty* collection literal has element type `Never`
        // even outside fluid mode, so `first([])` solves `T` to `Never` rather than leaking
        // `Unknown` into the call. Only emptiness qualifies — a non-empty literal's element
        // typevar also reaches the fallback below while a type context drives the solve, and
        // pinning it to `Never` there would discard the context's answer.
        let sound_empty_literal = elts.is_empty() && self.settings().sound_types;

        for (elts_index, elts) in elts.iter().enumerate() {
            // An unpacking expression for a dictionary.
            if let &[None, Some(value_expr)] = elts.as_slice() {
                let unpack_ty = infer_elt_expression(self, (1, value_expr, tcx));

                let Some((unpacked_key_ty, unpacked_value_ty)) =
                    unpack_ty.unpack_keys_and_items(db, env)
                else {
                    if let Some(builder) =
                        self.context.report_lint(&INVALID_ARGUMENT_TYPE, value_expr)
                    {
                        let mut diag = builder
                            .into_diagnostic("Argument expression after ** must be a mapping type");

                        diag.set_primary_annotation_message(format_args!(
                            "Found `{}`",
                            unpack_ty.display(db, env)
                        ));
                    }

                    continue;
                };

                let mut elt_tys = elt_tys.clone();
                if let Some((key_ty, value_ty)) = elt_tys.next_tuple() {
                    tuple_size_promotion_constraints.record_unpromotable_type(
                        db,
                        env,
                        key_ty.identity(self.db()),
                        unpacked_key_ty.promote_in(self.db(), env, self.file()),
                    );
                    tuple_size_promotion_constraints.record_unpromotable_type(
                        db,
                        env,
                        value_ty.identity(self.db()),
                        unpacked_value_ty.promote_in(self.db(), env, self.file()),
                    );

                    builder.infer(Type::TypeVar(key_ty), unpacked_key_ty).ok()?;

                    builder
                        .infer(Type::TypeVar(value_ty), unpacked_value_ty)
                        .ok()?;
                }

                continue;
            }

            // The inferred type of each element acts as an additional constraint on `T`.
            for (i, elt, elt_ty) in itertools::izip!(0.., elts, elt_tys.clone()) {
                let Some(elt) = elt else { continue };

                // Note that unlike when preferring the declared type, we use covariant type
                // assignments from the type context to potentially _narrow_ the inferred type,
                // by avoiding promotion.
                let elt_ty_identity = elt_ty.identity(self.db());

                // If the element is a starred expression, we want to apply the type context to each element
                // in the unpacked expression (which we will store as a tuple when inferring it). We
                // therefore wrap the type context in an `tuple[T, ...]` specialization.
                let elt_tcx = elt_tcx_constraints
                    .get(&elt_ty_identity)
                    .copied()
                    .map(|tcx| {
                        if elt.is_starred_expr() && collection_class != KnownClass::Dict {
                            Type::homogeneous_tuple(db, env, tcx)
                        } else {
                            tcx
                        }
                    });

                let inferred_elt_ty = pre_inferred_elt_tys
                    .as_ref()
                    .and_then(|inferred_elts| inferred_elts[elts_index][i])
                    .unwrap_or_else(|| {
                        infer_elt_expression(self, (i, elt, TypeContext::new(elt_tcx)))
                    });

                // Simplify the inference based on a non-covariant declared type.
                if let Some(elt_tcx) =
                    elt_tcx.filter(|_| !elt_tcx_variance[&elt_ty_identity].is_covariant())
                    && inferred_elt_ty.is_assignable_to(db, env, elt_tcx)
                {
                    continue;
                }

                // We promote element literal types in invariant position by default, unless they were
                // inferred with an explicit literal annotation. Fluid candidates retain literal
                // types until their first widening event.
                let inferred_elt_ty = if fluid_def.is_some() {
                    inferred_elt_ty
                } else {
                    // an *element* type is exactly what a container's layout is chosen
                    // from, so a module that asked for strict numerics has to get one
                    // here too — otherwise appending a `float` infers `list[int | float]`
                    // and the buffer is lost
                    //
                    // A covariant context is an upper bound, so promotion must not widen an
                    // otherwise compatible element beyond that bound. In particular, promoting
                    // an exact float introduces `int`, which is not assignable to an
                    // exact-float context.
                    let promoted_elt_ty = inferred_elt_ty.promote_in(self.db(), env, self.file());
                    if let Some(elt_tcx) = elt_tcx
                        && elt_tcx_variance[&elt_ty_identity].is_covariant()
                        && promoted_elt_ty != inferred_elt_ty
                        && !promoted_elt_ty.is_assignable_to(db, env, elt_tcx)
                        && inferred_elt_ty.is_assignable_to(db, env, elt_tcx)
                    {
                        inferred_elt_ty
                    } else {
                        promoted_elt_ty
                    }
                };

                let inferred_type_for_typevar = if elt.is_starred_expr() {
                    inferred_elt_ty
                        .iterate(db, env)
                        .homogeneous_element_type(db, env)
                } else {
                    inferred_elt_ty
                };

                tuple_size_promotion_constraints.record_inferred_expression_type(
                    db,
                    env,
                    elt_ty_identity,
                    elt,
                    inferred_type_for_typevar,
                );

                builder
                    .infer(Type::TypeVar(elt_ty), inferred_type_for_typevar)
                    .ok()?;
            }
        }

        let class_type = collection_alias
            .origin(self.db())
            .apply_specialization(db, |_| {
                builder.build_with(generic_context, |current_typevar, bounds| {
                    let Some(lower) = bounds.and_then(|bounds| bounds.lower) else {
                        // In fluid mode, an element typevar with no constraints comes from an
                        // empty collection literal (e.g. `a = []`). Solve it to `Never` — the
                        // precise element type of an empty collection — rather than the gradual
                        // `Unknown`; later uses widen it from there.
                        return (fluid_def.is_some() || sound_empty_literal).then_some(Type::Never);
                    };

                    // Fluid element types are promoted, same as non-fluid collection literals:
                    // a fluid binding widens across a promoted element type rather than an
                    // accumulating literal union.
                    //
                    // TODO(perf): retaining literals (`list[Literal[1, 2]]`) is the intended
                    // fluid behavior, but literal-parametrized generics blow up the cross-module
                    // constraint solver (~40x, ecosystem timeouts). Promoting here trades that
                    // precision for tractable performance until the solver cost is addressed;
                    // see the fluid-specialization performance investigation.
                    let lower = if is_empty_collection_type_context(tcx) {
                        // Constraints learned from later collection uses follow the same promotion
                        // policy as literal elements: promote element literal types in invariant
                        // position unless an explicit annotation made them unpromotable — and,
                        // like them, follow the file's numeric model, or a `float` element
                        // widens back to `int | float` and the buffer is lost
                        lower.promote_in(self.db(), env, self.file())
                    } else {
                        lower
                    };

                    let lower = if tuple_size_promotion_constraints
                        .allow(current_typevar.identity(self.db()))
                    {
                        lower.promote_tuple_size_in_union(db, env)
                    } else {
                        lower
                    };

                    let lower = if is_empty_collection_type_context(tcx) {
                        lower
                            // Promote singleton types to `T | Unknown` in inferred type parameters,
                            // so that e.g. `[None]` is inferred as `list[None | Unknown]`.
                            .promote_singletons_recursively(db, env)
                    } else {
                        lower
                    };

                    Some(lower)
                })
            });

        let creation = Type::from(class_type).to_instance_approximation(self.db(), env)?;

        if let Some(fluid_def) = fluid_def {
            // Combine the creation-time solution with the constraining events of the
            // binding's later uses, and record the creation-time type so that each use
            // can re-solve its own prefix of the events.
            return Some(self.fluid_eventual_type(
                fluid_def,
                identity_instance,
                generic_context,
                creation,
            ));
        }

        Some(creation)
    }

    /// Infer the type of the `iter` expression of the first comprehension.
    fn infer_first_comprehension_iter(&mut self, comprehensions: &[ast::Comprehension]) {
        let mut comprehensions_iter = comprehensions.iter();
        let Some(first_comprehension) = comprehensions_iter.next() else {
            unreachable!("Comprehension must contain at least one generator");
        };
        self.infer_maybe_standalone_expression(&first_comprehension.iter, TypeContext::default());
    }

    /// Derive the type context for a generator expression's yielded element from the expected type
    /// of the generator expression itself.
    ///
    /// We model the generator expression as a synthetic `GeneratorType[T, None, None]` or
    /// `AsyncGeneratorType[T, None]`, then ask constraint-set assignability to solve for `T` against
    /// the expected annotation. The solved `T` becomes the type context for the expression being
    /// yielded, so normal assignability handles protocols and unions like `Iterable[int] | None`
    /// without adding target-specific special cases here.
    fn generator_yield_type_context(
        &self,
        tcx: TypeContext<'db>,
        evaluation_mode: EvaluationMode,
    ) -> TypeContext<'db> {
        let db = self.db();
        let env = self.program_environment();
        let Some(annotation) = tcx.annotation() else {
            return TypeContext::default();
        };

        let yield_typevar = BoundTypeVarInstance::synthetic(
            db,
            env,
            Name::new_static("_GeneratorYieldT"),
            TypeVarVariance::Covariant,
        );
        let yield_ty = Type::TypeVar(yield_typevar);
        let none = Type::none(db, env);
        let generator_ty = if evaluation_mode.is_async() {
            KnownClass::AsyncGeneratorType.to_specialized_instance(db, env, &[yield_ty, none])
        } else {
            KnownClass::GeneratorType.to_specialized_instance(db, env, &[yield_ty, none, none])
        };

        let generic_context = GenericContext::from_typevar_instances(db, env, [yield_typevar]);
        let path_bounds = generator_ty.assignable_solutions_with_inferable(
            db,
            env,
            annotation,
            generic_context.inferable_typevars(db),
        );
        let constraints = ConstraintSetBuilder::new();
        let Solutions::Constrained(solutions) = path_bounds.solve(db, env, &constraints) else {
            return TypeContext::default();
        };

        let mut yield_tcx: Option<UnionAccumulator<'db>> = None;
        for solution in solutions {
            for binding in solution {
                if binding.bound_typevar != yield_typevar {
                    continue;
                }
                match &mut yield_tcx {
                    Some(accumulator) => {
                        accumulator.add(db, env, binding.solution);
                    }
                    None => yield_tcx = Some(UnionAccumulator::new(binding.solution)),
                }
            }
        }

        TypeContext::new(yield_tcx.map(|accumulator| accumulator.into_type(db, env)))
    }

    fn infer_generator_expression(
        &mut self,
        generator: &ast::ExprGenerator,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let ast::ExprGenerator {
            range: _,
            node_index: _,
            elt,
            generators,
            parenthesized: _,
        } = generator;

        self.infer_first_comprehension_iter(generators);

        let Some(scope_id) = self
            .index
            .try_node_scope(NodeWithScopeRef::GeneratorExpression(generator))
        else {
            return Type::unknown();
        };
        let evaluation_mode =
            EvaluationMode::from_is_async(scope_id.is_async_comprehension(self.index));
        let yield_tcx = self.generator_yield_type_context(tcx, evaluation_mode);
        let scope = scope_id.to_scope_id(self.db(), self.program_file());
        let inference = infer_scope_types(self.db(), scope, yield_tcx);
        self.extend_scope(inference);
        let yield_type = self.comprehension_element_type(elt, inference);

        if evaluation_mode.is_async() {
            KnownClass::AsyncGeneratorType.to_specialized_instance(
                db,
                env,
                &[yield_type, Type::none(db, env)],
            )
        } else {
            KnownClass::GeneratorType.to_specialized_instance(
                db,
                env,
                &[yield_type, Type::none(db, env), Type::none(db, env)],
            )
        }
    }

    fn comprehension_element_type(
        &self,
        element: &ast::Expr,
        inference: &ScopeInference<'db>,
    ) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let element_type = inference.expression_type(element);
        if element.is_starred_expr() {
            element_type
                .iterate(db, env)
                .homogeneous_element_type(db, env)
        } else {
            element_type
        }
    }

    /// Return a specialization of the collection class (list, dict, set) based on the type context and the inferred
    /// element / key-value types from the comprehension expression.
    fn infer_comprehension_specialization<const N: usize>(
        &mut self,
        collection_class: KnownClass,
        collection_expr: ast::ExprRef<'_>,
        elements: [Option<&ast::Expr>; N],
        inference: &ScopeInference<'db>,
        tcx: TypeContext<'db>,
    ) -> Option<Type<'db>> {
        let mut infer_element_ty =
            |_builder: &mut Self, (_, elt, _)| inference.expression_type(elt);

        self.infer_collection_literal(
            collection_class,
            Some(collection_expr),
            &[elements],
            &mut infer_element_ty,
            tcx,
        )
    }

    fn infer_list_comprehension_expression(
        &mut self,
        listcomp: &ast::ExprListComp,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let db = self.db();
        let ast::ExprListComp {
            range: _,
            node_index: _,
            elt,
            generators,
        } = listcomp;

        self.infer_first_comprehension_iter(generators);

        let Some(scope_id) = self
            .index
            .try_node_scope(NodeWithScopeRef::ListComprehension(listcomp))
        else {
            return Type::unknown();
        };
        let scope = scope_id.to_scope_id(self.db(), self.program_file());
        let inference = infer_scope_types(self.db(), scope, tcx);
        self.extend_scope(inference);

        self.infer_comprehension_specialization(
            KnownClass::List,
            listcomp.into(),
            [Some(elt)],
            inference,
            tcx,
        )
        .unwrap_or_else(|| {
            KnownClass::List.to_specialized_instance(
                db,
                self.program_environment(),
                &[Type::unknown()],
            )
        })
    }

    fn infer_set_comprehension_expression(
        &mut self,
        setcomp: &ast::ExprSetComp,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let db = self.db();
        let ast::ExprSetComp {
            range: _,
            node_index: _,
            elt,
            generators,
        } = setcomp;

        self.infer_first_comprehension_iter(generators);

        let Some(scope_id) = self
            .index
            .try_node_scope(NodeWithScopeRef::SetComprehension(setcomp))
        else {
            return Type::unknown();
        };
        let scope = scope_id.to_scope_id(self.db(), self.program_file());
        let inference = infer_scope_types(self.db(), scope, tcx);
        self.extend_scope(inference);

        self.infer_comprehension_specialization(
            KnownClass::Set,
            setcomp.into(),
            [Some(elt)],
            inference,
            tcx,
        )
        .unwrap_or_else(|| {
            KnownClass::Set.to_specialized_instance(
                db,
                self.program_environment(),
                &[Type::unknown()],
            )
        })
    }

    fn infer_dict_comprehension_expression(
        &mut self,
        dictcomp: &ast::ExprDictComp,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let db = self.db();
        let ast::ExprDictComp {
            range: _,
            node_index: _,
            key,
            value,
            generators,
        } = dictcomp;

        self.infer_first_comprehension_iter(generators);

        let Some(scope_id) = self
            .index
            .try_node_scope(NodeWithScopeRef::DictComprehension(dictcomp))
        else {
            return Type::unknown();
        };
        let scope = scope_id.to_scope_id(self.db(), self.program_file());
        let inference = infer_scope_types(self.db(), scope, tcx);
        self.extend_scope(inference);

        self.infer_comprehension_specialization(
            KnownClass::Dict,
            dictcomp.into(),
            [key.as_deref(), Some(value)],
            inference,
            tcx,
        )
        .unwrap_or_else(|| {
            KnownClass::Dict.to_specialized_instance(
                db,
                self.program_environment(),
                &[Type::unknown(), Type::unknown()],
            )
        })
    }

    fn infer_generator_expression_scope(
        &mut self,
        generator: &ast::ExprGenerator,
        tcx: TypeContext<'db>,
    ) {
        let db = self.db();
        let ast::ExprGenerator {
            range: _,
            node_index: _,
            elt,
            generators,
            parenthesized: _,
        } = generator;

        let elt_tcx = if elt.is_starred_expr() {
            tcx.map(|yield_ty| {
                KnownClass::Iterable.to_specialized_instance(
                    db,
                    self.program_environment(),
                    &[yield_ty],
                )
            })
        } else {
            tcx
        };
        self.infer_expression(elt, elt_tcx);
        self.infer_comprehensions(generators);
    }

    fn infer_list_comprehension_expression_scope(
        &mut self,
        listcomp: &ast::ExprListComp,
        tcx: TypeContext<'db>,
    ) {
        let ast::ExprListComp {
            range: _,
            node_index: _,
            elt,
            generators,
        } = listcomp;

        // Infer the element type using the outer type context.
        let elts = [[Some(elt.as_ref())]];
        let mut infer_elt_ty =
            |builder: &mut Self, (_, elt, tcx)| builder.infer_expression(elt, tcx);

        self.infer_collection_literal(
            KnownClass::List,
            Some(listcomp.into()),
            &elts,
            &mut infer_elt_ty,
            tcx,
        );

        self.infer_comprehensions(generators);
    }

    fn infer_set_comprehension_expression_scope(
        &mut self,
        setcomp: &ast::ExprSetComp,
        tcx: TypeContext<'db>,
    ) {
        let ast::ExprSetComp {
            range: _,
            node_index: _,
            elt,
            generators,
        } = setcomp;

        // Infer the element type using the outer type context.
        let elts = [[Some(elt.as_ref())]];
        let mut infer_elt_ty =
            |builder: &mut Self, (_, elt, tcx)| builder.infer_expression(elt, tcx);

        self.infer_collection_literal(
            KnownClass::Set,
            Some(setcomp.into()),
            &elts,
            &mut infer_elt_ty,
            tcx,
        );

        self.infer_comprehensions(generators);
    }

    fn infer_dict_comprehension_expression_scope(
        &mut self,
        dictcomp: &ast::ExprDictComp,
        tcx: TypeContext<'db>,
    ) {
        let ast::ExprDictComp {
            range: _,
            node_index: _,
            key,
            value,
            generators,
        } = dictcomp;

        if key.is_some() {
            // Infer the key and value types using the outer type context.
            let elts = [[key.as_deref(), Some(value.as_ref())]];
            let mut infer_elt_ty =
                |builder: &mut Self, (_, elt, tcx)| builder.infer_expression(elt, tcx);

            self.infer_collection_literal(
                KnownClass::Dict,
                Some(dictcomp.into()),
                &elts,
                &mut infer_elt_ty,
                tcx,
            );
        } else {
            // Dict-unpack comprehensions are typed by the outer expression inference. Inferring
            // them through the collection-literal helper here would report the same invalid
            // mapping diagnostic twice.
            self.infer_expression(value, TypeContext::default());
        }

        self.infer_comprehensions(generators);
    }

    fn infer_comprehensions(&mut self, comprehensions: &[ast::Comprehension]) {
        let mut comprehensions_iter = comprehensions.iter();
        let Some(first_comprehension) = comprehensions_iter.next() else {
            unreachable!("Comprehension must contain at least one generator");
        };
        self.infer_comprehension(first_comprehension, true);
        for comprehension in comprehensions_iter {
            self.infer_comprehension(comprehension, false);
        }
    }

    fn infer_comprehension(&mut self, comprehension: &ast::Comprehension, is_first: bool) {
        let db = self.db();
        let env = self.program_environment();
        let ast::Comprehension {
            range: _,
            node_index: _,
            target,
            iter,
            ifs,
            is_async: _,
        } = comprehension;

        self.infer_target(target, iter, &|builder, tcx| {
            // TODO: `infer_comprehension_definition` reports a diagnostic if `iter_ty` isn't iterable
            //  but only if the target is a name. We should report a diagnostic here if the target isn't a name:
            //  `[... for a.x in not_iterable]
            if is_first {
                infer_same_file_expression_type(builder.db(), builder.index.expression(iter), tcx)
            } else {
                builder.infer_maybe_standalone_expression(iter, tcx)
            }
            .iterate(db, env)
            .homogeneous_element_type(db, env)
        });

        for expr in ifs {
            let guard_ty = self.infer_maybe_standalone_expression(expr, TypeContext::default());
            // a guard whose type has no usable `__bool__` is reported like any other condition
            // site, and the basedpython condition lints are skipped for it — there is no
            // truthiness to reason about
            match guard_ty.try_bool(self.db(), env) {
                Ok(_) => self.check_condition(expr),
                Err(err) => err.report_diagnostic(&self.context, expr),
            }
        }
    }

    fn infer_comprehension_definition(
        &mut self,
        comprehension: &ComprehensionDefinitionKind<'db>,
        definition: Definition<'db>,
    ) {
        let db = self.db();
        let iterable = comprehension.iterable(self.module());
        let target = comprehension.target(self.module());

        let mut infer_iterable_type = || {
            let expression = self.index.expression(iterable);
            let result = infer_expression_types(self.db(), expression, TypeContext::default());
            let iterable_type = result.expression_type(iterable);
            let element_type = if comprehension.is_async() {
                None
            } else {
                self.fixed_length_iterable_element_type(iterable, |expr| {
                    result.expression_type(expr)
                })
            };

            // Two things are different if it's the first comprehension:
            // (1) We must lookup the `ScopedExpressionId` of the iterable expression in the outer scope,
            //     because that's the scope we visit it in in the semantic index builder
            // (2) We must *not* call `self.extend()` on the result of the type inference,
            //     because `ScopedExpressionId`s are only meaningful within their own scope, so
            //     we'd add types for random wrong expressions in the current scope
            if !(comprehension.is_first() && target.is_name_expr()) {
                self.extend_expression_unchecked(result);
            }

            (iterable_type, element_type)
        };

        let target_type = match comprehension.target_kind() {
            TargetKind::Sequence(unpack_position, unpack) => {
                let unpacked = infer_unpack_types(self.db(), unpack);
                if unpack_position == UnpackPosition::First {
                    self.context.extend(unpacked.diagnostics());
                }

                unpacked.expression_type(target)
            }
            TargetKind::Single => {
                let (iterable_type, element_type) = infer_iterable_type();

                report_iteration_over_character(&self.context, iterable_type, iterable.into());

                if let Some(element_type) = element_type {
                    element_type
                } else {
                    let env = self.program_environment();
                    iterable_type
                        .try_iterate_with_mode(
                            db,
                            env,
                            EvaluationMode::from_is_async(comprehension.is_async()),
                        )
                        .map(|tuple| tuple.homogeneous_element_type(db, env))
                        .unwrap_or_else(|err| {
                            err.report_diagnostic(&self.context, iterable_type, iterable.into());
                            err.fallback_element_type(db, env)
                        })
                }
            }
        };

        self.expressions.insert(target.into(), target_type);
        self.add_binding(target.into(), definition)
            .insert(self, target_type);
    }

    fn infer_named_expression(&mut self, named: &ast::ExprNamed) -> Type<'db> {
        // basedpython: a Named with `Invalid` ctx target is not a walrus —
        // it's an anon-NT field label, parameter-spec field, or kw subscription.
        // it has no walrus definition; just infer the value type.
        if let ast::Expr::Name(n) = named.target.as_ref()
            && matches!(n.ctx, ast::ExprContext::Invalid)
        {
            return self.infer_expression(&named.value, TypeContext::default());
        }
        // See https://peps.python.org/pep-0572/#differences-between-assignment-expressions-and-assignment-statements
        if named.target.is_name_expr() {
            let definition = self.index.expect_single_definition(named);
            let result = infer_definition_types(self.db(), definition);
            self.extend_definition(definition, result);
            result.binding_type(definition)
        } else {
            // For syntactically invalid targets, we still need to run type inference:
            self.infer_expression(&named.target, TypeContext::default());
            self.infer_expression(&named.value, TypeContext::default());
            Type::unknown()
        }
    }

    fn infer_named_expression_definition(
        &mut self,
        named: &'ast ast::ExprNamed,
        definition: Definition<'db>,
    ) -> Type<'db> {
        let ast::ExprNamed {
            range: _,
            node_index: _,
            target,
            value,
        } = named;

        let add = self.add_binding(named.target.as_ref().into(), definition);

        let ty = self.infer_expression(value, add.type_context());
        self.store_expression_type(target, ty);
        add.insert(self, ty)
    }

    /// basedpython: infers the type of a [statement expression](ast::ExprStatement).
    ///
    /// The wrapped statement is inferred as an ordinary statement. Its *value* was
    /// modelled by the semantic index as a synthetic place written at each of the
    /// statement's value positions, so the union of the branch types and whether
    /// the statement is exhaustive both fall out of ordinary place resolution:
    /// a value that is possibly undefined at this read means some path completes
    /// the statement without producing one.
    fn infer_statement_expression(&mut self, statement: &ast::ExprStatement) -> Type<'db> {
        let env = self.program_environment();
        // basedpython: a trailing lambda block's value is the call it stands for,
        // not a union of tail expressions, so it is neither collected nor subject
        // to the exhaustiveness check. the call is checked in the block's
        // decorators region, which records the type it produces
        if let Some(function) = statement.trailing_lambda() {
            let definition = self.index.expect_single_definition(function);
            self.infer_statement(&statement.stmt);
            return function_known_decorators(self.db(), definition)
                .trailing_lambda_return()
                .unwrap_or_else(Type::unknown);
        }

        self.infer_statement(&statement.stmt);

        // `raise`, `return`, `break` and `continue` never complete, so they have
        // no value position to bind and are not subject to the exhaustiveness
        // check
        if matches!(
            &*statement.stmt,
            ast::Stmt::Raise(_)
                | ast::Stmt::Return(_)
                | ast::Stmt::Break(_)
                | ast::Stmt::Continue(_)
        ) {
            return Type::Never;
        }

        let db = self.db();
        let file_scope_id = self.scope().file_scope_id(db);
        let use_def = self.index.use_def_map(file_scope_id);
        let use_id =
            ast::ExprRef::Statement(statement).scoped_use_id(db, db.program_file(self.file()));
        let place = place_from_bindings_with_reachability_cache(
            db,
            env,
            use_def.bindings_at_use(use_id),
            self.reachability_cache(),
        )
        .place;

        match place {
            Place::Defined(defined) if defined.definedness == Definedness::AlwaysDefined => {
                defined.ty
            }
            Place::Defined(defined) => {
                self.report_non_exhaustive_statement_expression(statement);
                defined.ty
            }
            // every branch diverges: no path reaches the value, so the expression
            // is `Never` rather than a value that went missing
            Place::Undefined
                if !use_def.bindings_at_use(use_id).any(|binding| {
                    evaluate_reachability(db, use_def, binding.reachability_constraint)
                        .may_be_true()
                }) =>
            {
                Type::Never
            }
            Place::Undefined => {
                self.report_non_exhaustive_statement_expression(statement);
                Type::unknown()
            }
        }
    }

    fn report_non_exhaustive_statement_expression(&self, statement: &ast::ExprStatement) {
        let Some(builder) = self
            .context
            .report_lint(&NON_EXHAUSTIVE_STATEMENT_EXPRESSION, statement)
        else {
            return;
        };
        let kind = match &*statement.stmt {
            ast::Stmt::If(_) => "`if`",
            ast::Stmt::Match(_) => "`match`",
            ast::Stmt::For(_) => "`for`",
            ast::Stmt::While(_) => "`while`",
            _ => "statement",
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "this {kind} expression can complete without producing a value"
        ));
        match &*statement.stmt {
            ast::Stmt::If(_) | ast::Stmt::Match(_) => {
                diagnostic.info("every branch must end in an expression, and the branches must cover every case");
            }
            ast::Stmt::For(_) | ast::Stmt::While(_) => {
                diagnostic.info(
                    "add an `else` clause to give the loop a value when it completes without `break`",
                );
            }
            _ => {}
        }
    }

    fn infer_if_expression(
        &mut self,
        if_expression: &ast::ExprIf,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let ast::ExprIf {
            range: _,
            node_index: _,
            test,
            body,
            orelse,
        } = if_expression;

        let test_ty = self.infer_maybe_standalone_expression(test, TypeContext::default());
        let (body_ty, orelse_ty) =
            if is_empty_collection_type_context(tcx) && is_collection_literal(body) {
                // Infer the peer branch first so the body can use its type as context.
                let orelse_ty = self.infer_expression(orelse, tcx);
                let body_ty = self.infer_expression_with_collection_literal_peer_context(
                    body,
                    tcx,
                    Some(orelse_ty),
                );
                (body_ty, orelse_ty)
            } else {
                let body_ty = self.infer_expression(body, tcx);
                let orelse_ty = self.infer_expression_with_collection_literal_peer_context(
                    orelse,
                    tcx,
                    Some(body_ty),
                );
                (body_ty, orelse_ty)
            };

        let truthiness = match test_ty.try_bool(self.db(), env) {
            Ok(truthiness) => {
                self.check_condition(test);
                truthiness
            }
            Err(err) => {
                err.report_diagnostic(&self.context, &**test);
                err.fallback_truthiness()
            }
        };

        match truthiness {
            Truthiness::AlwaysTrue => body_ty,
            Truthiness::AlwaysFalse => orelse_ty,
            Truthiness::Ambiguous => UnionType::from_two_elements(db, env, body_ty, orelse_ty),
        }
    }

    fn infer_lambda_body(&mut self, lambda_expression: &ast::ExprLambda, tcx: TypeContext<'db>) {
        self.infer_expression(&lambda_expression.body, tcx);
    }

    fn infer_lambda_expression(
        &mut self,
        lambda_expression: &ast::ExprLambda,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let ast::ExprLambda {
            range: _,
            node_index: _,
            parameters,
            returns,
            body: _,
        } = lambda_expression;

        // In stub files, default values may reference names that are defined later in the file.
        let in_stub = self.in_stub();
        let previous_deferred_state = std::mem::replace(&mut self.deferred_state, in_stub.into());

        // TODO: We could perform multi-inference here if there are multiple `Callable` annotations
        // in the union/intersection.
        let callable_tcx = if let Some(tcx) = tcx.annotation()
            && let Some(callable) = tcx.filter_union(db, Type::is_callable_type).as_callable()
        {
            match callable.signatures(self.db()).overloads.as_slice() {
                [signature] => Some(signature),
                // TODO: We could similarly perform multi-inference here if there are multiple overloads.
                _ => None,
            }
        } else {
            None
        };

        // Extract the annotated parameter types.
        //
        // Note that `Callable` annotations are only valid for positional parameters.
        let mut parameter_types = match callable_tcx {
            None => [].iter(),
            Some(signature) => signature.parameters().into_iter(),
        }
        .map(Parameter::annotated_type);

        // resolve parameter type: prefer explicit annotation, then callable_tcx, then (under
        // `sound-types`) the promoted type of the default, else unannotated
        let resolve_param_annotation = |builder: &mut Self,
                                        param: &ast::ParameterWithDefault,
                                        ctx_ty: Option<Type<'db>>,
                                        default_ty: Option<Type<'db>>|
         -> Option<Type<'db>> {
            if let Some(annotation) = &param.parameter.annotation {
                Some(
                    builder
                        .infer_annotation_expression(
                            annotation,
                            DeferredExpressionState::from(builder.defer_annotations()),
                        )
                        .inner_type(),
                )
            } else if let Some(ctx_ty) = ctx_ty {
                Some(ctx_ty)
            } else if crate::types::function::infers_unannotated_signatures(
                builder.db(),
                builder.file(),
            ) {
                // basedpython: mirrors the unannotated function parameter rule, so that a
                // lambda's own signature is checked at its call sites. a lambda body is a
                // single expression, so there is nothing else to read and no hole is opened
                default_ty.map(|default_ty| default_ty.promote(builder.db(), env))
            } else {
                None
            }
        };

        let parameters = if let Some(parameters) = parameters {
            let positional_only = parameters
                .posonlyargs
                .iter()
                .map(|param| {
                    let ctx_ty = parameter_types.next();
                    let default_ty = param.default().map(|default_expr| {
                        self.infer_expression(default_expr, TypeContext::default())
                            .replace_parameter_defaults(self.db(), env)
                    });
                    let parameter_base = Parameter::positional_only(Some(param.name().id.clone()))
                        .with_optional_default_type(default_ty);
                    if let Some(ty) = resolve_param_annotation(self, param, ctx_ty, default_ty) {
                        parameter_base.with_annotated_type(ty)
                    } else {
                        parameter_base
                    }
                })
                .collect::<Vec<_>>();
            let positional_or_keyword = parameters
                .args
                .iter()
                .map(|param| {
                    let ctx_ty = parameter_types.next();
                    let default_ty = param.default().map(|default_expr| {
                        self.infer_expression(default_expr, TypeContext::default())
                            .replace_parameter_defaults(self.db(), env)
                    });
                    let parameter_base = Parameter::positional_or_keyword(param.name().id.clone())
                        .with_optional_default_type(default_ty);
                    if let Some(ty) = resolve_param_annotation(self, param, ctx_ty, default_ty) {
                        parameter_base.with_annotated_type(ty)
                    } else {
                        parameter_base
                    }
                })
                .collect::<Vec<_>>();
            let variadic = parameters
                .vararg
                .as_ref()
                .map(|param| Parameter::variadic(param.name().id.clone()));
            // `Callable[[...], R]` parameter types only apply to positional
            // parameters — keyword-only parameters never consume from
            // `parameter_types`. The explicit annotation on a typed lambda
            // (basedpython) parameter still takes priority via
            // `resolve_param_annotation`
            let keyword_only = parameters
                .kwonlyargs
                .iter()
                .map(|param| {
                    let default_ty = param.default().map(|default_expr| {
                        self.infer_expression(default_expr, TypeContext::default())
                            .replace_parameter_defaults(self.db(), env)
                    });
                    let parameter_base = Parameter::keyword_only(param.name().id.clone())
                        .with_optional_default_type(default_ty);
                    if let Some(ty) = resolve_param_annotation(self, param, None, default_ty) {
                        parameter_base.with_annotated_type(ty)
                    } else {
                        parameter_base
                    }
                })
                .collect::<Vec<_>>();
            let keyword_variadic = parameters
                .kwarg
                .as_ref()
                .map(|param| Parameter::keyword_variadic(param.name().id.clone()));

            let parameters = positional_only
                .into_iter()
                .chain(positional_or_keyword)
                .chain(variadic)
                .chain(keyword_only)
                .chain(keyword_variadic);

            Parameters::from_annotation(db, env, parameters)
        } else {
            Parameters::empty()
        };

        self.deferred_state = previous_deferred_state;

        let Some(scope_id) = self
            .index
            .try_node_scope(NodeWithScopeRef::Lambda(lambda_expression))
        else {
            return Type::unknown();
        };

        let scope = scope_id.to_scope_id(self.db(), self.program_file());

        // explicit `-> return_type` annotation takes priority over Callable context
        let declared_return_ty = if let Some(returns_expr) = returns {
            let ty = self
                .infer_annotation_expression(
                    returns_expr,
                    DeferredExpressionState::from(self.defer_annotations()),
                )
                .inner_type();
            Some(ty)
        } else {
            None
        };

        let return_tcx = if let Some(ty) = declared_return_ty {
            TypeContext::new(Some(ty))
        } else if let Some(signature) = callable_tcx {
            match signature.return_ty {
                Type::Dynamic(DynamicType::Unknown) => TypeContext::new(None),
                _ => TypeContext::new(Some(signature.return_ty)),
            }
        } else {
            // TODO: Useful inference of a lambda's return type will require a different approach,
            // which does the inference of the body expression based on arguments at each call site,
            // rather than eagerly computing a return type without knowing the argument types.
            TypeContext::new(None)
        };

        let inference = infer_scope_types(self.db(), scope, return_tcx);
        self.extend_scope(inference);

        let return_ty = if let Some(ty) = declared_return_ty {
            ty
        } else {
            inference.expression_type(lambda_expression.body.as_ref())
        };
        Type::Callable(CallableType::new(
            self.db(),
            CallableSignature::single(Signature::new(parameters, return_ty)),
            CallableTypeKind::FunctionLike,
            CallableFunctionProvenance::ImplicitReturn,
        ))
    }

    /// Attempt to narrow a splatted dictionary argument based on the narrowed types of individual
    /// keys, if any.
    ///
    /// Returns the intersection between the dictionary type and a synthesized typed dict of any narrowed
    /// keys, or `None` otherwise.
    fn try_narrow_dict_kwargs(
        &self,
        argument_type: Type<'db>,
        argument: &'ast ast::ArgOrKeyword,
    ) -> Option<Type<'db>> {
        let env = self.program_environment();
        let db = self.db();
        let file_scope_id = self.scope().file_scope_id(db);
        let use_def = self.index.use_def_map(file_scope_id);

        let keyword = argument.as_variadic()?;

        if !argument_type
            .as_nominal_instance()?
            .has_known_class(db, KnownClass::Dict)
        {
            return None;
        }

        let definition_key = |definition: Definition<'_>| {
            let key = match definition.kind(db) {
                DefinitionKind::DictKeyAssignment(assignment) => assignment.key(self.module()),
                DefinitionKind::Assignment(assignment) => {
                    &assignment.target(self.module()).as_subscript_expr()?.slice
                }
                DefinitionKind::AnnotatedAssignment(assignment) => {
                    &assignment.target(self.module()).as_subscript_expr()?.slice
                }
                _ => return None,
            };

            Some(key.as_string_literal_expr()?.value.to_str())
        };

        // Collect the types of each distinct key.
        let mut elements: Vec<(&str, Type<'db>)> = Vec::new();
        for bindings in
            use_def.multi_bindings_at_use(keyword.scoped_use_id(db, self.program_file()))
        {
            let place = place_from_bindings_with_reachability_cache(
                db,
                env,
                bindings.clone(),
                self.reachability_cache(),
            );
            let Some(key) = place.first_definition.and_then(definition_key) else {
                continue;
            };

            if let Place::Defined(DefinedPlace {
                ty: field_ty,
                definedness: Definedness::AlwaysDefined,
                ..
            }) = place.place
            {
                elements.push((key, field_ty));
            }
        }

        if elements.is_empty() {
            return None;
        }

        // Synthesize overloads for `__getitem__` based on known dictionary elements.
        let getitem_overloads = elements.into_iter().map(|(name, ty)| {
            Signature::new(
                Parameters::standard([
                    Parameter::positional_only(Some(Name::new_static("self"))),
                    Parameter::positional_or_keyword(Name::new_static("key"))
                        .with_annotated_type(Type::string_literal(db, name)),
                ]),
                ty,
            )
        });

        let getitem_protocol = Type::protocol_with_methods(
            db,
            env,
            [(
                "__getitem__",
                CallableType::new(
                    db,
                    CallableSignature::from_overloads(getitem_overloads),
                    CallableTypeKind::FunctionLike,
                    CallableFunctionProvenance::None,
                ),
            )],
        );

        // Note that we return an intersection to preserve the original dictionary type,
        // as it may contain keys that were not explicitly assigned to.
        Some(IntersectionType::from_elements(
            db,
            env,
            [argument_type, getitem_protocol],
        ))
    }

    /// Infer the variadic argument types needed for call binding and emit the shared diagnostics
    /// for invalid `*args` and `**kwargs` inputs.
    fn prepare_call_arguments<'a>(
        &mut self,
        arguments: &'a ast::Arguments,
    ) -> CallArguments<'a, 'db> {
        let db = self.db();
        let env = self.program_environment();
        let call_arguments =
            CallArguments::from_arguments(arguments, |arg_or_keyword, splatted_value| {
                let ty = self.get_or_infer_expression(splatted_value, TypeContext::default());
                if let ast::ArgOrKeyword::Arg(argument) = arg_or_keyword
                    && argument.is_starred_expr()
                {
                    self.store_expression_type(argument, ty);
                } else if let Some(ty) = self.try_narrow_dict_kwargs(ty, arg_or_keyword) {
                    return ty;
                }

                ty
            });

        for arg in &arguments.args {
            if let ast::Expr::Starred(ast::ExprStarred { value, .. }) = arg {
                let iterable_type = self.expression_type(value);
                report_iteration_over_character(
                    &self.context,
                    iterable_type,
                    value.as_ref().into(),
                );
                if let Err(err) = iterable_type.try_iterate(self.db(), env) {
                    err.report_diagnostic(&self.context, iterable_type, value.as_ref().into());
                }
            }
        }

        for keyword in arguments
            .keywords
            .iter()
            .filter(|keyword| keyword.arg.is_none())
        {
            let mapping_type = self.expression_type(&keyword.value);

            if mapping_type.as_paramspec_typevar(self.db()).is_some()
                || mapping_type.unpack_keys_and_items(db, env).is_some()
            {
                continue;
            }

            let Some(builder) = self
                .context
                .report_lint(&INVALID_ARGUMENT_TYPE, &keyword.value)
            else {
                continue;
            };

            builder
                .into_diagnostic("Argument expression after ** must be a mapping type")
                .set_primary_annotation_message(format_args!(
                    "Found `{}`",
                    mapping_type.display(db, env)
                ));
        }

        call_arguments
    }

    // TODO: This should not be needed once we use constraint sets to track the usages of each
    // container literal across a scope.
    // https://github.com/astral-sh/ty/issues/3507
    fn collection_use_constraint_from_specialization(
        &self,
        identity_instance: Type<'db>,
        receiver_generic_context: Option<GenericContext<'db>>,
        call_specialization: Specialization<'db>,
    ) -> Option<Type<'db>> {
        let db = self.db();
        let env = self.program_environment();
        let constraint = identity_instance.apply_specialization(db, call_specialization);
        let Some(receiver_generic_context) = receiver_generic_context else {
            return Some(constraint);
        };

        // Method-local typevars describe requirements imposed by the method, not concrete element
        // types learned for the collection. Until collection-use constraints are represented as
        // projected constraint sets, avoid leaking those method-local typevars into the inferred
        // collection literal type.
        if any_over_type(db, env, constraint, false, |ty| {
            ty.as_typevar().is_some_and(|typevar| {
                !receiver_generic_context.contains(self.db(), typevar.identity(self.db()))
            })
        }) {
            return None;
        }

        Some(constraint)
    }

    /// a checked cast validates its value with `isinstance`, which can only
    /// test a class — so a target whose type arguments are erased at runtime
    /// (`list[int]`, unlike a user generic's `A[int]`) narrows to a claim that
    /// nothing verifies. a protocol target whose members can't be checked
    /// structurally (a member whose specialized type has no runtime spelling)
    /// has no runtime residue at all, so the whole cast — not just its
    /// arguments — is unverified.
    ///
    /// the wording describes what a runtime check *can* test rather than what
    /// the transpiler emits, since a provably-redundant check is elided
    fn report_erased_cast_argument(
        &mut self,
        type_arg: &ast::Expr,
        value_ty: Type<'db>,
        target: Type<'db>,
    ) {
        let env = self.program_environment();
        // a statically-proven upcast (`B[int]() cast list[int]`) verifies
        // nothing at runtime, so no argument claim is dropped and the lint
        // would be a false positive
        if cast_is_redundant(self.db(), env, value_ty, target) {
            return;
        }
        let db = self.db();
        // the cast shares the parametric `is` engine, so a target the engine can
        // decide in full assumes nothing: a reified type parameter compares its
        // runtime cell (`def f[T](x: list[T])` casting to `list[int]` lowers to
        // `T == int`), and a static fold needs no check at all
        if let Some(alias) = crate::types::reified_infer::parametric_cast_target(db, env, target)
            && matches!(
                crate::types::reified_infer::classify_parametric_is(
                    db,
                    env,
                    self.file(),
                    value_ty,
                    alias,
                    type_arg,
                ),
                crate::types::reified_infer::ParametricIsPlan::TokenEq(_)
                    | crate::types::reified_infer::ParametricIsPlan::Fold(_)
            )
        {
            return;
        }
        // a protocol whose data and method members are all spellable *is*
        // checked structurally, so it never reaches here; only a protocol with a
        // member whose specialized type has no runtime spelling (a callable
        // attribute) does, and it has no runtime residue — the cast degrades to
        // an unchecked `typing.cast`
        if cast_target_is_unverifiable_protocol(db, env, self.file(), target) {
            let Some(builder) = self.context.report_lint(&ERASED_CAST_ARGUMENT, type_arg) else {
                return;
            };
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "`{}` cannot be checked at runtime",
                target.display(db, env)
            ));
            diagnostic.info(
                "a protocol member with no runtime spelling has no residue; the cast is unchecked",
            );
            return;
        }
        if !erases_type_arguments(db, env, self.file(), target) {
            return;
        }
        let Some(builder) = self.context.report_lint(&ERASED_CAST_ARGUMENT, type_arg) else {
            return;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "Type arguments of `{}` are erased at runtime",
            target.display(db, env)
        ));
        match runtime_check_target(db, env, self.file(), target) {
            Some(shallow) => diagnostic.info(format_args!(
                "a runtime check can only test `{shallow}`; the type arguments are assumed"
            )),
            None => diagnostic.info("the type arguments are assumed"),
        }
    }

    /// Report a plain `cast` that is not a widening, and say whether it was one.
    ///
    /// The unsuffixed `cast` reinterprets a value without looking at it, so the
    /// only casts it can make truthfully are the ones the checker already
    /// proves: `int cast object`, or a cast to the value's own type. Casting
    /// *down* — `object cast int` — asserts something about the value that
    /// nothing verifies, and so must name its failure mode instead, with `cast!`
    /// (raises) or `cast?` (yields `None`).
    ///
    /// A gradual `Any` / `Unknown` value is not a subtype of a concrete target,
    /// so it is reported too: nothing at all is known about such a value, which
    /// is exactly when the runtime check is worth having.
    ///
    /// The returned flag is whether the cast was unsound, not whether a
    /// diagnostic was emitted — a suppressed report still stands in for the
    /// non-overlapping one the caller would otherwise reach for.
    fn report_unsound_cast(
        &mut self,
        cast_kind: ast::CastKind,
        call_expression: &ast::ExprCall,
        value_ty: Type<'db>,
        target: Type<'db>,
    ) -> bool {
        if cast_kind != ast::CastKind::Static {
            return false;
        }
        let env = self.program_environment();
        let db = self.db();
        if cast_is_redundant(db, env, value_ty, target) {
            return false;
        }
        let Some(builder) = self.context.report_lint(&UNSOUND_CAST, call_expression) else {
            return true;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "Cast from `{}` to `{}` is not a widening",
            value_ty.display(db, env),
            target.display(db, env)
        ));
        diagnostic.info("`cast` reinterprets the value without checking it");
        diagnostic.help("Use `cast!` to raise on a mismatch, or `cast?` to yield `None`");
        // the synthetic callee spans the `cast` keyword itself, so the suffix
        // goes straight after it. the edit is unsafe because it is a real change
        // in behaviour — a value the cast was quietly lying about now raises,
        // which is the point, but it is the author's call rather than a sweep's
        diagnostic.set_fix(Fix::unsafe_edit(Edit::insertion(
            ast::CastKind::Checked.suffix().to_owned(),
            call_expression.func.range().end(),
        )));
        true
    }

    /// Warn when a cast bridges two types that can never share a value. Such a
    /// cast is always futile: `cast!` raises and `cast?` yields `None`.
    /// `Any`/`Unknown` overlap everything, so those never fire.
    fn report_non_overlapping_cast(
        &mut self,
        value_arg: &ast::Expr,
        value_ty: Type<'db>,
        target: Type<'db>,
    ) {
        let env = self.program_environment();
        let db = self.db();
        if !value_ty.is_disjoint_from(db, env, target) {
            return;
        }
        let Some(builder) = self.context.report_lint(&NON_OVERLAPPING_CAST, value_arg) else {
            return;
        };
        builder.into_diagnostic(format_args!(
            "Cast from `{}` to `{}` is between non-overlapping types",
            value_ty.display(db, env),
            target.display(db, env)
        ));
    }

    /// Warn when an optional value is passed as an argument to a parameter typed
    /// `object`. `object` swallows the `None` arm silently, so the call is
    /// well-typed but discards the "could be absent" information the optional
    /// carried. Unlike a declared assignment — which narrows the target back to
    /// the optional type — a call argument really is consumed as `object` here,
    /// so this use site is where the loss becomes observable. `!` (unwrap) or
    /// `cast object` (make it explicit) are the intended alternatives.
    fn report_optional_object_arguments(&mut self, call: &ast::ExprCall, bindings: &Bindings<'db>) {
        let env = self.program_environment();
        if !self.is_basedpython_file() {
            return;
        }
        let db = self.db();
        let arguments: Vec<ast::ArgOrKeyword> = call.arguments.iter_source_order().collect();
        let Some(parameter_types) = bindings.single_overload_parameter_types(arguments.len())
        else {
            return;
        };
        for (argument, parameter_type) in arguments.iter().zip(parameter_types) {
            let Some(parameter_type) = parameter_type else {
                continue;
            };
            if !target_swallows_optional(db, env, parameter_type) {
                continue;
            }
            let value = argument.value();
            let argument_type = self.expression_type(value);
            if !is_optional_value(db, argument_type) {
                continue;
            }
            let Some(builder) = self.context.report_lint(&OPTIONAL_OBJECT_CONVERSION, value) else {
                continue;
            };
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "Optional `{}` is implicitly widened to `{}`",
                argument_type.display(db, env),
                parameter_type.display(db, env)
            ));
            diagnostic.help("Unwrap it with `!`, or convert explicitly with `cast object`");
        }
    }

    /// Warn when a `bool` argument is bound to a numeric parameter it only fits by
    /// subclassing `int`. Only the arguments written at the call site are walked,
    /// so a bound method's synthetic `self` — and every `bool` operand a binary
    /// operator forwards to `int.__add__` and friends — is out of scope, which is
    /// what we want: those are uses of a boolean *as* a boolean.
    fn report_bool_as_int_arguments(&mut self, call: &ast::ExprCall, bindings: &Bindings<'db>) {
        let arguments: Vec<ast::ArgOrKeyword> = call.arguments.iter_source_order().collect();
        let Some(parameter_types) = bindings.single_overload_parameter_types(arguments.len())
        else {
            return;
        };
        for (argument, parameter_type) in arguments.iter().zip(parameter_types) {
            let Some(parameter_type) = parameter_type else {
                continue;
            };
            let value = argument.value();
            report_bool_as_int(
                &self.context,
                value,
                self.expression_type(value),
                parameter_type,
            );
        }
    }

    /// basedpython: report a `*args` argument whose value is not known to have the
    /// number of elements the call needs.
    ///
    /// This is the call-site half of [`REFUTABLE_UNPACKING`]. `f(*values)` binds the
    /// parameters positionally out of `values`, so a `tuple[int, ...]` that turns out to
    /// hold three elements is a `TypeError` against a two-parameter function, exactly as
    /// `a, b = values` is a `ValueError`. ty matches a splat of known length element by
    /// element and reports the ordinary arity errors; a splat of unknown length is
    /// instead assumed to fill whatever is left, which is what makes it silent.
    fn report_refutable_splat_arguments(&mut self, call: &ast::ExprCall, bindings: &Bindings<'db>) {
        let db = self.db();
        let env = self.program_environment();
        for (argument_index, argument) in call.arguments.iter_source_order().enumerate() {
            let ast::ArgOrKeyword::Arg(ast::Expr::Starred(starred)) = argument else {
                continue;
            };
            let value_ty = self.expression_type(&starred.value);

            // iterating a union collapses its members into one homogeneous element type,
            // which loses the very thing this check reads: a union of fixed-length tuples
            // has a bounded length, even though the collapsed spec is variable-length. so
            // ask each member on its own, as unpacking assignments do
            let members = match value_ty {
                Type::Union(union) => union.elements(db),
                _ => std::slice::from_ref(&value_ty),
            };

            for member_ty in members.iter().copied() {
                // a value we cannot iterate is reported as such, and its fallback says
                // nothing about a real length
                let Ok(tuple) = member_ty.try_iterate(db, env) else {
                    continue;
                };
                let length = tuple.len();
                if !length.is_variable()
                    || !refutable_unpacking_applies(db, member_ty, tuple.as_ref())
                {
                    continue;
                }
                let Some(demand) = bindings.splat_parameter_demand(argument_index) else {
                    continue;
                };
                // the splat always yields at least its own minimum, so it only falls short
                // of the required parameters when there are more of them than that
                if demand.maximum.is_none() && demand.required <= length.minimum() {
                    continue;
                }
                let Some(builder) = self.context.report_lint(&REFUTABLE_UNPACKING, starred) else {
                    continue;
                };
                builder.into_diagnostic(format_args!(
                    "`{value}` may not have {expected}, which would raise `TypeError` \
                     when unpacked into this call",
                    value = member_ty.display(db, env),
                    expected = display_required_elements(demand.required, demand.maximum),
                ));
            }
        }
    }

    fn infer_call_expression(
        &mut self,
        call_expression: &ast::ExprCall,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let env = self.program_environment();
        // basedpython's infix casts parse as `ExprCall { cast_kind: Some(..),
        // func: Name("cast"), arguments: [type, value] }`. The synthetic `cast`
        // name is unresolved by design, so dispatch on the kind: infer the
        // target as a type expression and walk the value for declarations
        if let Some(cast_kind) = call_expression.cast_kind
            && let [type_arg, value_arg] = &*call_expression.arguments.args
        {
            let value_ty = self.infer_expression(value_arg, TypeContext::default());
            let target = self.infer_type_expression(type_arg);
            // only a form that verifies at runtime can drop a type-argument
            // claim; the plain `cast` never reaches a check in the first place
            if cast_kind.verifies_at_runtime() {
                self.report_erased_cast_argument(type_arg, value_ty, target);
            }
            // an unsound plain `cast` is already the sharper report — a
            // non-overlapping one would only repeat that the value is not the
            // target, and the two would stack on the same expression
            if !self.report_unsound_cast(cast_kind, call_expression, value_ty, target) {
                self.report_non_overlapping_cast(value_arg, value_ty, target);
            }
            return match cast_kind {
                // the plain `cast` and `cast!` both hand back the target: the
                // first because the value already was one, the second because
                // it raised if it wasn't
                ast::CastKind::Static | ast::CastKind::Checked => target,
                // `cast?` yields the value or `None`
                ast::CastKind::Try => {
                    UnionType::from_elements(self.db(), env, [target, Type::none(self.db(), env)])
                }
            };
        }

        // basedpython carries the call's expected type into the callee so a bare
        // constructor resolves context-sensitively (`s: Shape = Circle(2.0)`);
        // nothing else reads it, and a python file is left exactly as it was
        let callee_tcx = if self.is_basedpython_file() {
            tcx.for_callee()
        } else {
            TypeContext::default()
        };
        let callable_type =
            self.infer_maybe_standalone_expression(&call_expression.func, callee_tcx);

        // basedpython `a?.b()`: the `?.` short-circuit covers the call too, matching the
        // `None if a is None else a.b()` lowering, so call the present-receiver callable and
        // let the `None` ride out to the end of the chain
        let (callable_type, in_chain) =
            self.basedpython_chain_receiver(&call_expression.func, callable_type);

        let return_type = self.infer_call_expression_impl(call_expression, callable_type, tcx);
        let return_type =
            self.basedpython_symbolic_call(call_expression, callable_type, return_type);

        self.basedpython_chain_result(call_expression, return_type, in_chain)
    }

    /// basedpython: keep a method call on a symbolic receiver symbolic.
    ///
    /// This is the value-level counterpart of the [`DeferredOperation::Call`] a type
    /// expression builds, the way `-i` is the counterpart of `-> -I` and `x.a` of `T.a`.
    /// Without it a body could never satisfy `-> s.startswith("foo")`: the annotation names
    /// the operation while the body would name its reduced form, so `True` and
    /// `s.startswith("foo")` would be equally acceptable — which is to say neither would be
    /// checked.
    ///
    /// The call itself has already been inferred, so its arguments are checked and its
    /// diagnostics reported exactly as they would be; only the *result* becomes the pending
    /// operation, which re-folds against the receiver a specialization supplies.
    fn basedpython_symbolic_call(
        &mut self,
        call_expression: &ast::ExprCall,
        callable_type: Type<'db>,
        return_type: Type<'db>,
    ) -> Type<'db> {
        let env = self.program_environment();
        if !self.is_basedpython_file()
            || !call_expression.arguments.keywords.is_empty()
            || call_expression
                .arguments
                .args
                .iter()
                .any(ast::Expr::is_starred_expr)
        {
            return return_type;
        }
        let ast::Expr::Attribute(method) = &*call_expression.func else {
            return return_type;
        };
        let Some(receiver) = self.try_expression_type(&method.value) else {
            return return_type;
        };
        // the receiver has to be symbolic on its own, not merely to mention a type
        // parameter: every method of a generic class binds one, and deferring those would
        // buy nothing but a symbolic form of the answer already computed. `Self` is that
        // same case — the receiver binding substitutes it at every call site, so a call on
        // it is already answered in terms of the class's own type parameters
        if !is_symbolic_operand(receiver)
            || receiver
                .as_typevar()
                .is_some_and(|typevar| typevar.typevar(self.db()).is_self(self.db()))
        {
            return return_type;
        }

        let mut operands = Vec::with_capacity(call_expression.arguments.args.len() + 1);
        operands.push(callable_type);
        for arg in &call_expression.arguments.args {
            operands.push(self.expression_type(arg));
        }
        let deferred = DeferredType::build(
            self.db(),
            env,
            &DeferredOperation::Call,
            operands.into_boxed_slice(),
        );
        // a call that turned out not to defer was already answered by the inference above,
        // which saw the arguments in context; re-deriving it from the operands could only
        // lose information
        if deferred.is_deferred() {
            deferred
        } else {
            return_type
        }
    }

    /// basedpython: a constructor call names the class it builds, so the value it
    /// produces has *exactly* that runtime class — `final A`, not merely `A`.
    ///
    /// This is the constructor counterpart of literal inference: `1` is inferred
    /// as `Literal[1]` and widens to `int` when a declaration is inferred from it,
    /// and `A()` is inferred as `final A` and widens the same way (see the
    /// `Type::Restricted` arm of `apply_type_mapping_impl`). It only applies when
    /// the callee is the class itself — a `type[A]` variable may hold a subclass —
    /// and when the call really did build one, so a `__new__` or metaclass
    /// `__call__` returning something else keeps its own return type.
    fn basedpython_exact_construction(
        &self,
        callable_type: Type<'db>,
        return_type: Type<'db>,
    ) -> Type<'db> {
        let env = self.program_environment();
        if !self.is_basedpython_file() {
            return return_type;
        }
        let db = self.db();
        let constructed = match callable_type {
            Type::ClassLiteral(class) => class,
            Type::GenericAlias(alias) => ClassType::Generic(alias).class_literal(db),
            _ => return return_type,
        };
        let Type::NominalInstance(instance) = return_type else {
            return return_type;
        };
        if instance.class(db, env).class_literal(db) != constructed {
            return return_type;
        }
        RestrictedType::from_type_expression(db, env, TypeModifier::Final, return_type)
    }

    fn infer_empty_list_or_set_constructor(
        &mut self,
        collection_class: KnownClass,
        call_expression: &ast::ExprCall,
        tcx: TypeContext<'db>,
    ) -> Option<Type<'db>> {
        let elements: [[Option<&ast::Expr>; 1]; 0] = [];
        let mut infer_element_ty = |_: &mut Self, _| Type::unknown();

        self.infer_collection_literal(
            collection_class,
            Some(call_expression.into()),
            &elements,
            &mut infer_element_ty,
            tcx,
        )
    }

    /// Infers a truthiness-refined `range` instance for literal built-in `range(...)` calls.
    ///
    /// The refinement only records whether the constructed range is statically non-empty. Dynamic
    /// arguments, keyword arguments, starred arguments, shadowed `range` callables, and invalid
    /// literal forms fall back to the ordinary `range` instance.
    ///
    /// This uses the argument types inferred by normal call binding; it does not re-infer
    /// arguments just to compute the refinement.
    ///
    /// ```python
    /// range(3)        # known non-empty
    /// range(3, 0, -1) # known non-empty
    /// range(n)        # ordinary range
    /// ```
    fn infer_builtin_range_instance_type(
        &self,
        callable_type: Type<'db>,
        arguments: &ast::Arguments,
        call_arguments: &CallArguments<'_, 'db>,
    ) -> Option<Type<'db>> {
        let Type::ClassLiteral(class) = callable_type else {
            return None;
        };
        if !class.is_known(self.db(), KnownClass::Range)
            || !arguments.keywords.is_empty()
            || arguments.args.iter().any(ast::Expr::is_starred_expr)
        {
            return None;
        }

        let int_literal = |argument_index: usize| {
            call_arguments
                .argument_types(argument_index)?
                .get_default()?
                .as_int_literal()
        };

        let is_non_empty = match arguments.args.len() {
            1 => int_literal(0)? > 0,
            2 => int_literal(0)? < int_literal(1)?,
            3 => {
                let start = int_literal(0)?;
                let stop = int_literal(1)?;
                let step = int_literal(2)?;

                match step.cmp(&0) {
                    std::cmp::Ordering::Greater => start < stop,
                    std::cmp::Ordering::Less => start > stop,
                    std::cmp::Ordering::Equal => return None,
                }
            }
            _ => return None,
        };

        Some(Type::KnownInstance(KnownInstanceType::Range {
            is_non_empty,
        }))
    }

    /// basedpython: fold a `float(...)` call over literal arguments into the float
    /// literal it constructs, so that `float("inf")` infers the same `inf` type the
    /// `float.inf` annotation spells — python's only way to write the special values.
    ///
    /// Falls back to the ordinary `float` instance for keyword, starred, or
    /// non-literal arguments, and for any string rust declines to parse (python
    /// additionally accepts underscores and surrounding whitespace, so a string
    /// rust parses is always one python parses too).
    fn infer_basedpython_float_literal_call(
        &self,
        callable_type: Type<'db>,
        arguments: &ast::Arguments,
        call_arguments: &CallArguments<'_, 'db>,
    ) -> Option<Type<'db>> {
        if !self.is_basedpython_file() {
            return None;
        }
        let Type::ClassLiteral(class) = callable_type else {
            return None;
        };
        if !class.is_known(self.db(), KnownClass::Float)
            || !arguments.keywords.is_empty()
            || arguments.args.iter().any(ast::Expr::is_starred_expr)
        {
            return None;
        }

        let value = match arguments.args.len() {
            0 => 0.0,
            1 => {
                let kind = call_arguments
                    .argument_types(0)?
                    .get_default()?
                    .as_literal_value_kind()?;
                match kind {
                    LiteralValueTypeKind::String(string) => {
                        string.value(self.db()).parse::<f64>().ok()?
                    }
                    kind => binary_expressions::as_f64_value(kind)?,
                }
            }
            _ => return None,
        };

        Some(Type::float_literal(value))
    }

    /// basedpython: infer the positional arguments of a django lookup method
    /// that spell a `__` lookup as an expression (`filter(author.name == "x")`),
    /// reporting whether any did.
    ///
    /// The names in such an argument are field paths, not values, so they are
    /// inferred here rather than through the ordinary load path — the leading
    /// name takes the field's type (a relation's target model instance), and the
    /// segments after it are ordinary member accesses on it. The comparison
    /// itself is a `bool`, exactly as the source spells it; whether the value
    /// suits the field is [`check_django_queryset_call`]'s question, since the
    /// keyword form asks it there too.
    ///
    /// [`check_django_queryset_call`]: Self::check_django_queryset_call
    fn prepare_django_lookup_expressions(
        &mut self,
        callable_type: Type<'db>,
        arguments: &ast::Arguments,
    ) -> bool {
        let env = self.program_environment();
        if !self.is_basedpython_file() {
            return false;
        }
        let db = self.db();
        let Some(model) = django::lookup_call_model(db, env, callable_type) else {
            return false;
        };
        let lookups =
            django::lookup_expressions(db, env, self.file(), self.scope(), model, arguments);
        if lookups.is_empty() {
            return false;
        }
        for lookup in lookups {
            let mut ty = lookup.root_type;
            for (index, node) in lookup.path.iter().enumerate() {
                match node {
                    ast::Expr::Attribute(attribute) if index > 0 => {
                        ty = ty
                            .member(db, env, attribute.attr.as_str())
                            .place
                            .ignore_possibly_undefined()
                            .unwrap_or_else(Type::unknown);
                    }
                    ast::Expr::Subscript(subscript) => {
                        // the key is part of the keyword's name rather than a
                        // value, but it is still source that wants a type
                        self.infer_expression(&subscript.slice, TypeContext::default());
                        // a json key reads back arbitrary json, which is what the
                        // stubs say the field itself reads back too
                        ty = Type::any();
                    }
                    _ => {}
                }
                self.store_expression_type(node, ty);
            }
            self.infer_maybe_standalone_expression(lookup.value, TypeContext::default());
            self.store_expression_type(lookup.argument, KnownClass::Bool.to_instance(db, env));
        }
        true
    }

    /// Validate a django `Manager`/`QuerySet` method call against the bound
    /// model's fields: lookup kwargs (`filter`, `get`, …), `create()` kwargs,
    /// and literal field-name arguments (`order_by`, `only`, …). No-op for any
    /// call that isn't a recognized queryset method on a resolved model.
    fn check_django_queryset_call(
        &self,
        bound_method: crate::types::BoundMethodType<'db>,
        call_expression: &ast::ExprCall,
    ) {
        let env = self.program_environment();
        let db = self.db();
        let method_name = bound_method.function(db).name(db);
        let Some(kind) = django::queryset_method_kind(method_name.as_str()) else {
            return;
        };
        let Some(model) =
            django::queryset_or_manager_model(db, env, bound_method.self_instance(db))
        else {
            return;
        };
        let model_name = model.name(db);

        let report_unknown = |range: TextRange, model_name: &str, segment: &str, key: &str| {
            if let Some(builder) = self.context.report_lint(&INVALID_FIELD_LOOKUP, range) {
                if key == segment {
                    builder.into_diagnostic(format_args!(
                        "Model `{model_name}` has no field `{segment}`"
                    ));
                } else {
                    builder.into_diagnostic(format_args!(
                        "Model `{model_name}` has no field `{segment}` (in lookup `{key}`)"
                    ));
                }
            }
        };

        match kind {
            django::QuerysetMethodKind::Lookup | django::QuerysetMethodKind::Create => {
                let is_create = kind == django::QuerysetMethodKind::Create;
                for keyword in &call_expression.arguments.keywords {
                    let Some(arg) = &keyword.arg else {
                        continue; // `**kwargs` unpacking — can't check statically
                    };
                    let key = arg.as_str();
                    if django::is_method_own_keyword(method_name.as_str(), key) {
                        continue;
                    }
                    let resolution = if is_create {
                        django::resolve_create_kwarg(db, env, model, key)
                    } else {
                        django::resolve_lookup(db, env, model, key)
                    };
                    match resolution {
                        django::FieldResolution::Unknown { model, segment } => {
                            report_unknown(keyword.range(), &model, &segment, key);
                        }
                        django::FieldResolution::Resolved {
                            operand: Some(operand),
                        } => {
                            // a lookup on `field=None` is always valid (isnull
                            // semantics); a `create()` assignment is not
                            let operand = if is_create {
                                operand
                            } else {
                                UnionType::from_two_elements(db, env, operand, Type::none(db, env))
                            };
                            let value_ty = self.expression_type(&keyword.value);
                            if !value_ty.is_assignable_to(db, env, operand) {
                                if let Some(builder) =
                                    self.context.report_lint(&INVALID_FIELD_LOOKUP, keyword)
                                {
                                    builder.into_diagnostic(format_args!(
                                        "Value for `{key}` has type `{}`, \
                                         but `{}` expects `{}`",
                                        value_ty.display(db, env),
                                        model_name,
                                        operand.display(db, env),
                                    ));
                                }
                            }
                        }
                        django::FieldResolution::Resolved { operand: None } => {}
                    }
                }

                // basedpython: the same checks against the lookups the call's
                // positional arguments spell as expressions
                if !is_create && self.is_basedpython_file() {
                    for lookup in django::lookup_expressions(
                        db,
                        env,
                        self.file(),
                        self.scope(),
                        model,
                        &call_expression.arguments,
                    ) {
                        let range = lookup.argument.range();
                        match django::resolve_lookup(db, env, model, &lookup.key) {
                            django::FieldResolution::Unknown { model, segment } => {
                                report_unknown(range, &model, &segment, &lookup.key);
                            }
                            django::FieldResolution::Resolved {
                                operand: Some(operand),
                            } => {
                                let operand = UnionType::from_two_elements(
                                    db,
                                    env,
                                    operand,
                                    Type::none(db, env),
                                );
                                let value_ty = self.expression_type(lookup.value);
                                if !value_ty.is_assignable_to(db, env, operand)
                                    && let Some(builder) =
                                        self.context.report_lint(&INVALID_FIELD_LOOKUP, range)
                                {
                                    builder.into_diagnostic(format_args!(
                                        "Value for `{}` has type `{}`, but `{model_name}` expects `{}`",
                                        lookup.key,
                                        value_ty.display(db, env),
                                        operand.display(db, env),
                                    ));
                                }
                            }
                            django::FieldResolution::Resolved { operand: None } => {}
                        }
                    }
                }
            }
            django::QuerysetMethodKind::FieldNames => {
                for arg in &call_expression.arguments.args {
                    let ast::Expr::StringLiteral(literal) = arg else {
                        continue; // only literal field names are checkable
                    };
                    let name = literal.value.to_str();
                    if let django::FieldResolution::Unknown { model, segment } =
                        django::resolve_field_name(db, env, model, name)
                    {
                        report_unknown(arg.range(), &model, &segment, &segment);
                    }
                }
            }
        }
    }

    /// Refine a call to one of the `re` module functions from the capture groups
    /// of its pattern argument, and report a pattern that `re.compile` would
    /// reject.
    fn check_regex_function_call(
        &self,
        function: FunctionType<'db>,
        overload: &mut Binding<'db>,
        call_expression: &ast::ExprCall,
    ) {
        let env = self.program_environment();
        let db = self.db();
        if file_to_module(db, function.program_file(db).resolver_file(db))
            .and_then(|module| module.known(db))
            != Some(KnownModule::Re)
        {
            return;
        }
        let Some(call) = regex::RegexCall::from_name(function.name(db).as_str()) else {
            return;
        };
        let [Some(pattern_ty), ..] = overload.parameter_types() else {
            return;
        };
        let pattern_ty = *pattern_ty;

        // an already-compiled pattern brought its groups with it
        let (groups, any_str) = if let Some(groups) = regex::groups_of(db, pattern_ty) {
            let Some(any_str) = regex::any_str_of(db, env, pattern_ty) else {
                return;
            };
            (groups, any_str)
        } else {
            let Some((text, any_str)) = regex::pattern_source(db, env, pattern_ty) else {
                return;
            };
            let flags = overload
                .signature
                .parameters()
                .keyword_by_name("flags")
                .and_then(|(index, _)| {
                    call_expression
                        .arguments
                        .find_argument_value("flags", index)
                });
            let Some(verbose) = self.regex_verbose_flag(flags) else {
                return;
            };
            match regex::analyze(&text, verbose) {
                regex::PatternAnalysis::Groups(parsed) => {
                    (regex::RegexGroups::from_parsed(db, &parsed), any_str)
                }
                regex::PatternAnalysis::Invalid(error) => {
                    let anchor = call_expression
                        .arguments
                        .find_argument_value("pattern", 0)
                        .map_or(AnyNodeRef::from(call_expression), AnyNodeRef::from);
                    if let Some(builder) = self.context.report_lint(&INVALID_REGEX, anchor) {
                        builder.into_diagnostic(error.message());
                    }
                    overload.set_return_type(Type::unknown());
                    return;
                }
                regex::PatternAnalysis::Unknown => return,
            }
        };

        overload.set_return_type(regex::refined_return(
            db,
            env,
            call,
            groups,
            any_str,
            overload.return_ty,
        ));
    }

    /// Refine a `re.Pattern` or `re.Match` method call from the capture groups
    /// its receiver is carrying.
    fn check_regex_method_call(
        &self,
        bound_method: crate::types::BoundMethodType<'db>,
        overload: &mut Binding<'db>,
        call_expression: &ast::ExprCall,
    ) {
        let env = self.program_environment();
        let db = self.db();
        let receiver = bound_method.self_instance(db);
        let (Some(groups), Some(any_str)) = (
            regex::groups_of(db, receiver),
            regex::any_str_of(db, env, receiver),
        ) else {
            return;
        };
        let name = bound_method.function(db).name(db).as_str();

        if regex::is_pattern(db, receiver) {
            if let Some(call) = regex::RegexCall::from_name(name) {
                overload.set_return_type(regex::refined_return(
                    db,
                    env,
                    call,
                    groups,
                    any_str,
                    overload.return_ty,
                ));
            }
            return;
        }

        let Some(member) = regex::MatchMember::from_name(name) else {
            return;
        };
        let arguments = &call_expression.arguments;
        // `*args` or a keyword would leave us guessing which group is meant
        if !arguments.keywords.is_empty() || arguments.args.iter().any(ast::Expr::is_starred_expr) {
            return;
        }

        match member {
            regex::MatchMember::Group => {
                // `m.group()` with no argument is the whole match
                let Some((first, rest)) = arguments.args.split_first() else {
                    overload.set_return_type(any_str);
                    return;
                };
                let mut types = Vec::with_capacity(arguments.args.len());
                for argument in std::iter::once(first).chain(rest) {
                    let Some(key) = self.regex_group_key(argument) else {
                        return;
                    };
                    let Ok(ty) = regex::group_type(db, env, groups, any_str, key) else {
                        self.report_no_such_regex_group(argument.into(), key);
                        overload.set_return_type(Type::unknown());
                        return;
                    };
                    types.push(ty);
                }
                overload.set_return_type(match types[..] {
                    [single] => single,
                    _ => Type::heterogeneous_tuple(db, env, types),
                });
            }
            regex::MatchMember::Groups => {
                let unset = arguments.args.first().map(|it| self.expression_type(it));
                overload.set_return_type(regex::groups_type(db, env, groups, any_str, unset));
            }
            regex::MatchMember::GroupDict => {
                let unset = arguments.args.first().map(|it| self.expression_type(it));
                if let Some(ty) = regex::group_dict_type(db, env, groups, any_str, unset) {
                    overload.set_return_type(ty);
                }
            }
            regex::MatchMember::Position => {
                if let Some(argument) = arguments.args.first()
                    && let Some(key) = self.regex_group_key(argument)
                    && regex::group_type(db, env, groups, any_str, key).is_err()
                {
                    self.report_no_such_regex_group(argument.into(), key);
                }
            }
        }
    }

    /// The group a `Match` member's argument names, if it names one statically.
    fn regex_group_key<'a>(&'a self, argument: &ast::Expr) -> Option<regex::GroupKey<'a>> {
        let ty = self.expression_type(argument);
        if let Some(literal) = ty.as_string_literal() {
            return Some(regex::GroupKey::Name(literal.value(self.db())));
        }
        u32::try_from(ty.as_int_literal()?)
            .ok()
            .map(regex::GroupKey::Number)
    }

    fn report_no_such_regex_group(&self, anchor: AnyNodeRef<'_>, key: regex::GroupKey<'_>) {
        if let Some(builder) = self.context.report_lint(&INVALID_REGEX, anchor) {
            builder.into_diagnostic(format_args!("No such group: {key}"));
        }
    }

    /// The capture groups a `sub`/`subn` call should hand to a callable
    /// replacement, which needs them before its arguments are inferred.
    fn regex_substitution_groups(
        &self,
        callable_type: Type<'db>,
        arguments: &ast::Arguments,
    ) -> Option<regex::RegexGroups<'db>> {
        let env = self.program_environment();
        let db = self.db();
        let is_substitution =
            |name: &str| regex::RegexCall::from_name(name) == Some(regex::RegexCall::Substitute);

        match callable_type {
            Type::BoundMethod(bound_method)
                if is_substitution(bound_method.function(db).name(db).as_str()) =>
            {
                regex::groups_of(db, bound_method.self_instance(db))
            }
            Type::FunctionLiteral(function)
                if is_substitution(function.name(db).as_str())
                    && file_to_module(db, function.program_file(db).resolver_file(db))
                        .and_then(|module| module.known(db))
                        == Some(KnownModule::Re) =>
            {
                let pattern = arguments.find_argument_value("pattern", 0)?;
                let pattern_ty = self
                    .speculate_without_diagnostics()
                    .infer_expression(pattern, TypeContext::default());
                if let Some(groups) = regex::groups_of(db, pattern_ty) {
                    return Some(groups);
                }
                let (text, _) = regex::pattern_source(db, env, pattern_ty)?;
                // no signature has been matched yet, so the `flags` parameter is
                // located by its position in `re.sub`/`re.subn` directly
                let verbose = self.regex_verbose_flag(arguments.find_argument_value("flags", 4))?;
                match regex::analyze(&text, verbose) {
                    regex::PatternAnalysis::Groups(parsed) => {
                        Some(regex::RegexGroups::from_parsed(db, &parsed))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Whether the `flags` argument of a `re` call turns on verbose mode.
    ///
    /// `None` means we could not tell, which has to mean "no refinement":
    /// verbose mode changes how the pattern itself tokenizes, so a guess here
    /// would produce confidently wrong group types.
    fn regex_verbose_flag(&self, flags: Option<&ast::Expr>) -> Option<bool> {
        let Some(flags) = flags else {
            return Some(false);
        };
        // flags are combined with `|`, and the combination is verbose if any
        // operand is. any other operator leaves us unable to say
        if let ast::Expr::BinOp(binop) = flags {
            if binop.op != ast::Operator::BitOr {
                return None;
            }
            let left = self.regex_verbose_flag(Some(&binop.left))?;
            let right = self.regex_verbose_flag(Some(&binop.right))?;
            return Some(left || right);
        }
        // the argument may not have been inferred yet — the repl callable of
        // `re.sub` is refined before the later arguments are visited — so fall
        // back to a speculative builder when there is no stored type
        let ty = self.try_expression_type(flags).unwrap_or_else(|| {
            self.speculate_without_diagnostics()
                .infer_expression(flags, TypeContext::default())
        });
        regex::flag_is_verbose(self.db(), ty)
    }

    fn infer_call_expression_impl(
        &mut self,
        call_expression: &ast::ExprCall,
        callable_type: Type<'db>,
        call_expression_tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let return_type =
            self.infer_call_expression_inner(call_expression, callable_type, call_expression_tcx);
        self.basedpython_exact_construction(callable_type, return_type)
    }

    fn infer_call_expression_inner(
        &mut self,
        call_expression: &ast::ExprCall,
        callable_type: Type<'db>,
        call_expression_tcx: TypeContext<'db>,
    ) -> Type<'db> {
        fn report_missing_implicit_constructor_call<'db>(
            context: &InferContext<'db, '_>,
            callable_type: Type<'db>,
            call_expression: &ast::ExprCall,
            bindings: &Bindings<'db>,
        ) {
            let db = context.db();
            let env = context.program_environment();
            if bindings.has_implicit_dunder_new_is_possibly_unbound() {
                if let Some(builder) =
                    context.report_lint(&POSSIBLY_MISSING_IMPLICIT_CALL, call_expression)
                {
                    builder.into_diagnostic(format_args!(
                        "Method `__new__` on type `{}` may be missing.",
                        callable_type.display(db, env),
                    ));
                }
            }

            if bindings.has_implicit_dunder_init_is_possibly_unbound() {
                if let Some(builder) =
                    context.report_lint(&POSSIBLY_MISSING_IMPLICIT_CALL, call_expression)
                {
                    builder.into_diagnostic(format_args!(
                        "Method `__init__` on type `{}` may be missing.",
                        callable_type.display(db, env),
                    ));
                }
            }
        }

        let db = self.db();
        let env = self.program_environment();
        let ast::ExprCall {
            range_start: _,
            node_index: _,
            func,
            arguments,
            cast_kind: _,
            is_string_tag: _,
        } = call_expression;

        // Semantic indexing recognizes only bare empty constructor calls. Confirm that the name
        // still resolves to the corresponding builtin before using later collection constraints.
        let collection_initializer_class = if arguments.is_empty()
            && self
                .index
                .try_expression(call_expression)
                .and_then(|expression| expression.assigned_to(self.db()))
                .is_some()
            && let Some(name) = func.as_name_expr()
            && let Some(known_class) = callable_type
                .as_class_literal()
                .and_then(|class| class.known(self.db()))
            && matches!(
                (name.id.as_str(), known_class),
                ("list", KnownClass::List) | ("set", KnownClass::Set) | ("dict", KnownClass::Dict)
            ) {
            Some(known_class)
        } else {
            None
        };

        if callable_type
            .as_class_literal()
            .is_some_and(|class_literal| class_literal.is_known(self.db(), KnownClass::Dict))
            && let Some(ty) = self.infer_keyword_only_dict_call(
                func,
                arguments,
                (collection_initializer_class == Some(KnownClass::Dict))
                    .then_some(call_expression.into()),
                call_expression_tcx,
            )
        {
            return ty;
        }

        // Handle 3-argument `type(name, bases, dict)`.
        if let Type::ClassLiteral(class) = callable_type
            && class.is_known(self.db(), KnownClass::Type)
        {
            return self.infer_builtins_type_call(call_expression, None);
        }

        // Handle `types.new_class(name, bases, ...)`.
        if let Some(function) = callable_type.as_function_literal()
            && function.is_known(self.db(), KnownFunction::NewClass)
        {
            return self.infer_new_class_call(call_expression, None);
        }

        // Handle `typing.NamedTuple(typename, fields)` and `collections.namedtuple(typename, field_names)`.
        if let Some(namedtuple_kind) = NamedTupleKind::from_type(self.db(), callable_type) {
            return self.infer_namedtuple_call_expression(call_expression, None, namedtuple_kind);
        }

        // Handle `Enum(name, members)`.
        if let Some(base_class) = enum_call::enum_functional_call_base(self.db(), callable_type)
            && let Some(ty) = self.infer_enum_call_expression(call_expression, None, base_class)
        {
            return ty;
        }

        if let Some(typed_dict_module) = TypedDictModule::from_type(self.db(), callable_type) {
            return self.infer_typeddict_call_expression(call_expression, None, typed_dict_module);
        }

        if callable_type == Type::SpecialForm(SpecialFormType::TypeForm) {
            return self.infer_type_form_call_expression(call_expression);
        }

        if callable_type.is_notimplemented(self.db()) {
            if let Some(builder) = self
                .context
                .report_lint(&CALL_NON_CALLABLE, call_expression)
            {
                let mut diagnostic = builder.into_diagnostic("`NotImplemented` is not callable");
                diagnostic.annotate(
                    self.context
                        .secondary(&**func)
                        .message("Did you mean `NotImplementedError`?"),
                );
                diagnostic.set_concise_message(
                    "`NotImplemented` is not callable - did you mean `NotImplementedError`?",
                );
            }
            return Type::unknown();
        }

        let class = match callable_type {
            Type::ClassLiteral(class) => Some(ClassType::NonGeneric(class)),
            Type::GenericAlias(generic) => Some(ClassType::Generic(generic)),
            Type::SubclassOf(subclass) => subclass.subclass_of().into_class(db, env),
            _ => None,
        };

        if let Some(class) = class
            && class.is_typed_dict(db)
        {
            return self.infer_typed_dict_constructor(
                callable_type,
                class,
                call_expression,
                call_expression_tcx,
            );
        }

        // basedpython: a django lookup written as an expression names fields
        // rather than values, so its own inference has to happen before the
        // arguments are inferred as ordinary expressions
        // Prepare `TypedDict` constructor calls before variadic argument setup so field-directed
        // value inference becomes canonical before `**kwargs` expressions are inferred.
        let has_prepared_typed_dict_constructor = class
            .filter(|class| class.is_typed_dict(self.db()))
            .map(|class| {
                let typed_dict = TypedDictType::new(class);
                let form = typed_dict::TypedDictConstructorForm::from_arguments(arguments);
                self.prepare_typed_dict_constructor(
                    typed_dict,
                    form,
                    arguments,
                    func.as_ref().into(),
                );
            })
            .is_some();

        let has_django_lookup_expressions =
            self.prepare_django_lookup_expressions(callable_type, arguments);

        // We don't call `Type::try_call`, because we want to perform type inference on the
        // arguments after matching them to parameters, but before checking that the argument types
        // are assignable to any parameter annotations.
        let mut call_arguments = self.prepare_call_arguments(arguments);

        // Special handling for `TypedDict` method calls
        if let ast::Expr::Attribute(ast::ExprAttribute { value, attr, .. }) = func.as_ref() {
            let value_type = self.expression_type(value);
            let method_name = attr.id.as_str();

            if let Type::TypedDict(typed_dict_ty) = value_type
                && matches!(method_name, "get" | "pop" | "setdefault")
                && !arguments.args.is_empty()
                && let Some(first_arg) = (
                    // Validate the key argument for `TypedDict` methods
                    arguments.args.first()
                )
                && let Some(key) = (match first_arg {
                    ast::Expr::StringLiteral(ast::ExprStringLiteral {
                        value: key_literal, ..
                    }) => Some(key_literal.to_str()),
                    _ => self
                        .speculate_without_diagnostics()
                        .get_or_infer_expression(first_arg, TypeContext::default())
                        .as_string_literal()
                        .map(|key_literal| key_literal.value(self.db())),
                })
            {
                let items = typed_dict_ty.items(self.db());
                let is_declared = items.contains_key(key);

                if let Some(field) = typed_dict_ty.item(self.db(), key) {
                    // Key exists - check if it's a `pop()` on a required field
                    if is_declared && method_name == "pop" && field.is_required() {
                        report_cannot_pop_required_field_on_typed_dict(
                            &self.context,
                            first_arg.into(),
                            Type::TypedDict(typed_dict_ty),
                            key,
                        );
                        return Type::unknown();
                    }

                    if !is_declared
                        && method_name == "get"
                        && arguments.keywords.is_empty()
                        && matches!(arguments.args.len(), 1 | 2)
                    {
                        let default_ty = if let Some(default) = arguments.args.get(1) {
                            self.get_or_infer_expression(
                                default,
                                TypeContext::new(Some(field.declared_ty)),
                            )
                        } else {
                            Type::none(db, env)
                        };
                        return UnionType::from_two_elements(
                            db,
                            env,
                            field.declared_ty,
                            default_ty,
                        );
                    }

                    if !is_declared && field.is_read_only() {
                        let mutation = match method_name {
                            "pop"
                                if arguments.keywords.is_empty()
                                    && matches!(arguments.args.len(), 1 | 2) =>
                            {
                                Some(("pop", "from"))
                            }
                            "setdefault"
                                if arguments.keywords.is_empty() && arguments.args.len() == 2 =>
                            {
                                Some(("set default for", "on"))
                            }
                            _ => None,
                        };
                        if let Some((action, preposition)) = mutation {
                            if let Some(builder) =
                                self.context.report_lint(&INVALID_ARGUMENT_TYPE, first_arg)
                            {
                                builder.into_diagnostic(format_args!(
                                    "Cannot {action} read-only extra item \
                                    \"{key}\" {preposition} TypedDict `{}`",
                                    Type::TypedDict(typed_dict_ty).display(db, env),
                                ));
                            }
                            return Type::unknown();
                        }
                    }

                    // Unknown literal keys are concrete extra items, so mutating operations can
                    // use their extra-items type even when arbitrary `str` keys are unsafe.
                    if !is_declared && !field.is_read_only() {
                        match method_name {
                            "pop"
                                if arguments.keywords.is_empty()
                                    && matches!(arguments.args.len(), 1 | 2) =>
                            {
                                return arguments.args.get(1).map_or(
                                    field.declared_ty,
                                    |default| {
                                        UnionType::from_two_elements(
                                            db,
                                            env,
                                            field.declared_ty,
                                            self.get_or_infer_expression(
                                                default,
                                                TypeContext::new(Some(field.declared_ty)),
                                            ),
                                        )
                                    },
                                );
                            }
                            "setdefault"
                                if arguments.keywords.is_empty() && arguments.args.len() == 2 =>
                            {
                                let default = &arguments.args[1];
                                let default_ty = self.get_or_infer_expression(
                                    default,
                                    TypeContext::new(Some(field.declared_ty)),
                                );
                                TypedDictKeyAssignment {
                                    context: &self.context,
                                    typed_dict: typed_dict_ty,
                                    full_object_ty: None,
                                    key,
                                    value_ty: default_ty,
                                    typed_dict_node: value.as_ref().into(),
                                    key_node: first_arg.into(),
                                    value_node: default.into(),
                                    assignment_kind: TypedDictAssignmentKind::Constructor,
                                    emit_diagnostic: true,
                                }
                                .validate();
                                return field.declared_ty;
                            }
                            _ => {}
                        }
                    }
                } else if method_name != "get" {
                    // Key not found, report error with suggestion and return early
                    let key_ty = Type::string_literal(self.db(), key);
                    report_invalid_key_on_typed_dict(
                        &self.context,
                        first_arg.into(),
                        first_arg.into(),
                        Type::TypedDict(typed_dict_ty),
                        None,
                        key_ty,
                        items,
                    );
                    // Return `Unknown` to prevent the overload system from generating its own error
                    return Type::unknown();
                }
            }
        }

        if let Type::FunctionLiteral(function) = callable_type {
            // Make sure that the `function.definition` is only called when the function is defined
            // in the same file as the one we're currently inferring the types for. This is because
            // the `definition` method accesses the semantic index, which could create a
            // cross-module AST dependency.
            if function.file(self.db()) == self.file()
                && function.definition(self.db()).scope(self.db()) == self.scope()
            {
                self.called_functions.insert(function);
            }

            // Warn when `final()` is called as a function (not a decorator).
            // Type checkers cannot interpret this usage and will not prevent subclassing.
            if function.is_known(self.db(), KnownFunction::Final) {
                if let Some(builder) = self
                    .context
                    .report_lint(&INEFFECTIVE_FINAL, call_expression)
                {
                    let mut diagnostic = builder.into_diagnostic(
                        "Type checkers will not prevent subclassing \
                        when `final()` is called as a function",
                    );
                    diagnostic.info("Use `@final` as a decorator on a class or method instead");
                }
            }
        }

        // Check for unsound calls to abstract classmethods/staticmethods on class objects
        match callable_type {
            Type::BoundMethod(bound_method) => {
                let function = bound_method.function(self.db());
                if let Some(class) = bound_method.self_instance(self.db()).to_class_type(db) {
                    if function.is_classmethod(self.db())
                        && function.as_abstract_method(self.db(), class).is_some()
                        && function.has_trivial_body(self.db())
                    {
                        report_call_to_abstract_method(
                            &self.context,
                            call_expression,
                            function,
                            "classmethod",
                        );
                    }
                }
            }
            Type::FunctionLiteral(function) if function.is_staticmethod(self.db()) => {
                if let ast::Expr::Attribute(ast::ExprAttribute { value, .. }) = func.as_ref() {
                    let value_type = self.expression_type(value);
                    if let Some(class) = value_type.to_class_type(db) {
                        if function.as_abstract_method(self.db(), class).is_some()
                            && function.has_trivial_body(self.db())
                        {
                            report_call_to_abstract_method(
                                &self.context,
                                call_expression,
                                function,
                                "staticmethod",
                            );
                        }
                    }
                }
            }
            _ => {}
        }

        if let Some(class) = class {
            // It might look odd here that we emit an error for class-literals and generic aliases but not
            // `type[]` types. But it's deliberate! The typing spec explicitly mandates that `type[]` types
            // can be called even though class-literals cannot. This is because even though a protocol class
            // `SomeProtocol` is always an abstract class, `type[SomeProtocol]` can be a concrete subclass of
            // that protocol -- and indeed, according to the spec, type checkers must disallow abstract
            // subclasses of the protocol to be passed to parameters that accept `type[SomeProtocol]`.
            // <https://typing.python.org/en/latest/spec/protocol.html#type-and-class-objects-vs-protocols>.
            if !callable_type.is_subclass_of()
                && let Some(protocol) = class.into_protocol_class(self.db())
            {
                report_attempted_protocol_instantiation(&self.context, call_expression, protocol);
            }

            // Inference of correctly-placed `TypeVar`, `ParamSpec`, `NewType`, and
            // `TypeAliasType` definitions is done in `infer_legacy_typevar`,
            // `infer_paramspec`, `infer_newtype_expression`, and
            // `infer_typealiastype_call`, and doesn't use the full call-binding
            // machinery. If we reach here, it means that someone is trying to
            // instantiate one of these in an invalid context.
            match class.known(self.db()) {
                Some(KnownClass::TypeVar | KnownClass::ExtensionsTypeVar) => {
                    if let Some(builder) = self
                        .context
                        .report_lint(&INVALID_LEGACY_TYPE_VARIABLE, call_expression)
                    {
                        builder.into_diagnostic(
                            "A `TypeVar` definition must be a simple variable assignment",
                        );
                    }
                }
                Some(KnownClass::ParamSpec | KnownClass::ExtensionsParamSpec) => {
                    if let Some(builder) = self
                        .context
                        .report_lint(&INVALID_PARAMSPEC, call_expression)
                    {
                        builder.into_diagnostic(
                            "A `ParamSpec` definition must be a simple variable assignment",
                        );
                    }
                }
                Some(KnownClass::TypeVarTuple | KnownClass::ExtensionsTypeVarTuple) => {
                    if let Some(builder) = self
                        .context
                        .report_lint(&INVALID_LEGACY_TYPE_VARIABLE, call_expression)
                    {
                        builder.into_diagnostic(
                            "A `TypeVarTuple` definition must be a simple variable assignment",
                        );
                    }
                }
                Some(KnownClass::NewType) => {
                    if let Some(builder) =
                        self.context.report_lint(&INVALID_NEWTYPE, call_expression)
                    {
                        builder.into_diagnostic(
                            "A `NewType` definition must be a simple variable assignment",
                        );
                    }
                }
                Some(KnownClass::TypeAliasType) => {
                    if let Some(builder) = self
                        .context
                        .report_lint(&INVALID_TYPE_ALIAS_TYPE, call_expression)
                    {
                        builder.into_diagnostic(
                            "A `TypeAliasType` definition must be a simple variable assignment",
                        );
                    }
                }
                _ => {}
            }
        }
        let mut bindings =
            self.bindings_for_call(callable_type)
                .match_parameters(db, env, &call_arguments);

        // basedpython: fill unmatched `context` parameters from the `context`
        // declarations visible at this call site, before check/report. gated
        // to the callables the transpiler can also inject for (a plain
        // function or bound method — `single_signature`), so a call the
        // checker accepts is always one the lowering completes
        if matches!(
            callable_type,
            Type::FunctionLiteral(_) | Type::BoundMethod(_)
        ) {
            bindings.resolve_context_arguments(
                self.db(),
                env,
                self.scope(),
                call_expression.range().start(),
            );
        }

        report_missing_implicit_constructor_call(
            &self.context,
            callable_type,
            call_expression,
            &bindings,
        );

        // `re.sub(pattern, repl, …)` hands its callable replacement a `Match`,
        // and the pattern's groups have to reach that callable's parameter
        // *before* a lambda is inferred against it
        let substitution_groups = self.regex_substitution_groups(callable_type, arguments);

        let bindings_result = self.infer_and_check_argument_types(
            ArgumentsIter::from_ast(arguments),
            &mut call_arguments,
            &mut |builder, (_, expr, tcx)| {
                let tcx = match substitution_groups {
                    Some(groups) => {
                        tcx.map(|ty| regex::attach_groups(builder.db(), env, ty, groups))
                    }
                    None => tcx,
                };
                if has_prepared_typed_dict_constructor || has_django_lookup_expressions {
                    builder.get_or_infer_expression(expr, tcx)
                } else {
                    builder.infer_expression(expr, tcx)
                }
            },
            &mut bindings,
            call_expression_tcx,
        );

        // Record the constraints for the receiver of a bound method call before
        // bailing out on call-binding errors: the constraints are solved against the
        // receiver's identity specialization and widen a fluid receiver, which can
        // resolve the very error the call produced against the narrower
        // flow-sensitive receiver type.
        if self.fluid_specializations_enabled()
            && let ast::Expr::Attribute(attribute @ ast::ExprAttribute { value, .. }) =
                func.as_ref()
        {
            let value_type = self.expression_type(value);

            if let Some(collection_def) = self.index.fluid_candidate_binding(value)
                && let Some((collection_literal, _)) =
                    value_type.class_specialization(self.db(), env)
            {
                let identity_instance = Type::instance(
                    self.db(),
                    env,
                    collection_literal.identity_specialization(self.db()),
                );
                let collection_generic_context = collection_literal.generic_context(self.db());

                // the identity-receiver probe is speculative: suppress its
                // diagnostics (a basedpython extension member with a bracket
                // bound legitimately fails to resolve on the identity
                // specialization, and that must not surface as an error)
                let mut identity_bindings = self
                    .speculate_without_diagnostics()
                    .infer_attribute_load_impl(attribute, identity_instance)
                    .unwrap_or_else(|recovery_ty| recovery_ty)
                    .bindings(db, env)
                    .match_parameters(db, env, &call_arguments)
                    // Perform inference against the type variables on the receiver's generic context.
                    .with_generic_context(self.db(), collection_generic_context);

                let call_result = self
                    .speculate_without_diagnostics()
                    .infer_and_check_argument_types(
                        ArgumentsIter::from_ast(arguments),
                        &mut call_arguments,
                        // TODO: The argument types have already been inferred and stored in `call_arguments`.
                        // However, `value` would have been inferred to a be a collection with `Divergent`
                        // element types, meaning the type context for a given argument, by which the inferred
                        // type is keyed, may not be the same as the type context we get here. It is not immediately
                        // clear how to retrieve those types, and so we just re-infer the argument expressions
                        // for simplicity.
                        &mut |builder, (_, expr, tcx)| builder.infer_expression(expr, tcx),
                        &mut identity_bindings,
                        call_expression_tcx,
                    );

                if call_result.is_ok() {
                    let db = self.db();
                    for call_specialization in identity_bindings
                        .iter_flat()
                        .flat_map(CallableBinding::matching_overloads)
                        .filter_map(|(_, identity_overload)| {
                            identity_overload.specialization(db, env)
                        })
                    {
                        // Record the constraints on the receiver's generic context formed by
                        // the arguments to this bound method call.
                        let Some(constraints) = self.collection_use_constraint_from_specialization(
                            identity_instance,
                            collection_generic_context,
                            call_specialization,
                        ) else {
                            continue;
                        };

                        self.collection_use_constraints
                            .entry(collection_def)
                            .or_default()
                            .insert(constraints);
                    }
                }
            }
        }

        let mut bindings = match bindings_result {
            Ok(()) => bindings,
            Err(_) => {
                bindings.report_diagnostics(&self.context, call_expression.into());
                let return_ty = bindings.return_type(self.db(), env);
                self.record_unsolved_typevar_call(call_expression, return_ty, &bindings);
                return return_ty;
            }
        };

        // basedpython: `str`, `repr`, `format` and `print` put a value's
        // rendering in front of someone, so a value with no rendering of its
        // own is worth mentioning here as much as in an f-string
        if let Some(stringifying) = format::Stringifying::of(self.db(), callable_type) {
            format::check_stringifying_call(&self.context, stringifying, arguments, |expr| {
                self.try_expression_type(expr)
            });
        }

        // A call whose return type mentions the typevars solved from a fluid argument
        // hands the caller a new observer of that argument's specialization.
        self.record_fluid_return_observers(arguments, &mut bindings);

        // basedpython: a reified generic must be specialized (`f[...]`) before
        // it is called — the specialization is a runtime step. a bare call is
        // accepted only when the transpiler can inject that step: every type
        // parameter must solve, from the (fully spelled-out) arguments or its
        // pep 696 default, to a type with a runtime spelling. the shared
        // `reified_infer` query makes this decision and the injection from the
        // same inputs, so the two cannot diverge. a reified *method*
        // (`obj.m()`) is the same: its underlying function carries the reified
        // type parameters. a reified classmethod is reported at the def site,
        // so it is skipped here
        let reified_target = match callable_type {
            Type::FunctionLiteral(function) => Some(function),
            Type::BoundMethod(bound_method) => Some(bound_method.function(self.db())),
            _ => None,
        };
        if self.is_basedpython_file()
            && let Some(function) =
                reified_target.filter(|function| !function.is_classmethod(self.db()))
            && function.is_unspecialized_reified(self.db())
        {
            let has_unpacked_arguments = arguments.args.iter().any(ast::Expr::is_starred_expr)
                || arguments
                    .keywords
                    .iter()
                    .any(|keyword| keyword.arg.is_none());
            let inference_failure = if has_unpacked_arguments {
                Some(None)
            } else {
                reified_infer::inferred_call_type_arguments(
                    self.db(),
                    env,
                    self.file(),
                    callable_type,
                    function,
                    &call_arguments,
                )
                .err()
                .map(Some)
            };
            if let Some(failure) = inference_failure
                && let Some(builder) = self
                    .context
                    .report_lint(&UNSPECIALIZED_REIFIED_GENERIC, call_expression)
            {
                let name = function.name(self.db());
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "Cannot call reified generic function `{name}` \
                     without explicit specialization"
                ));
                match failure {
                    None => diagnostic.info(format_args!(
                        "the inferred specialization cannot be injected through \
                         unpacked arguments — specialize with `{name}[...]`"
                    )),
                    Some(ReifiedInferenceError::Unsolved(parameter)) => {
                        diagnostic.info(format_args!(
                            "reified type parameter `{parameter}` cannot be inferred \
                             from the arguments — specialize with `{name}[...]`"
                        ));
                    }
                    Some(ReifiedInferenceError::Unspellable(parameter, ty)) => {
                        diagnostic.info(format_args!(
                            "inferred type `{}` for type parameter `{parameter}` has \
                             no runtime spelling — specialize with `{name}[...]`",
                            ty.display(self.db(), env),
                        ));
                    }
                    Some(ReifiedInferenceError::NoBinding) => {
                        diagnostic.info(format_args!(
                            "the specialization cannot be inferred from this call — \
                             specialize with `{name}[...]`"
                        ));
                    }
                }
            }
        }

        self.report_optional_object_arguments(call_expression, &bindings);
        self.report_bool_as_int_arguments(call_expression, &bindings);
        self.report_refutable_splat_arguments(call_expression, &bindings);

        if let Some(class) = class {
            pydantic::report_discarded_extra_arguments(&self.context, class, arguments, &bindings);
        }

        for binding in bindings.iter_flat_mut() {
            let binding_type = binding.callable_type;
            for (_, overload) in binding.matching_overloads_mut() {
                match binding_type {
                    Type::FunctionLiteral(function_literal) => {
                        if let Some(known_function) = function_literal.known(self.db()) {
                            known_function.check_call(
                                &self.context,
                                overload,
                                &call_arguments,
                                call_expression,
                            );
                        }
                        self.check_regex_function_call(function_literal, overload, call_expression);
                    }
                    Type::ClassLiteral(class) => {
                        if let Some(known_class) = class.known(self.db()) {
                            known_class.check_call(
                                &self.context,
                                self.index,
                                overload,
                                call_expression,
                            );
                        }
                    }
                    Type::BoundMethod(bound_method) => {
                        self.check_django_queryset_call(bound_method, call_expression);
                        self.check_regex_method_call(bound_method, overload, call_expression);
                    }
                    Type::Never => {
                        // In unreachable sections of code, we infer `Never` for symbols that were
                        // defined outside the unreachable part. We still want to emit revealed-type
                        // diagnostics in these sections, so check on the name of the callable here
                        // and assume that it's actually `typing.reveal_type`.
                        let is_reveal_type = match func.as_ref() {
                            ast::Expr::Name(name) => name.id == "reveal_type",
                            ast::Expr::Attribute(attr) => {
                                attr.attr.id == "reveal_type" && is_dotted_name(func)
                            }
                            _ => false,
                        };
                        if is_reveal_type && let Some(first_arg) = arguments.args.first() {
                            let revealed_ty = self.expression_type(first_arg);
                            let declared_ty = declared_type_at_load(
                                self.db(),
                                env,
                                self.context.program_file(),
                                ast::ExprRef::from(first_arg),
                                revealed_ty,
                            );
                            report_revealed_type(
                                &self.context,
                                revealed_ty,
                                declared_ty,
                                first_arg,
                            );
                        }
                    }
                    _ => {}
                }
            }
        }

        // `range(...)` always constructs a `range`, but with literal arguments we can preserve
        // whether that range is statically non-empty on the constructed instance itself.
        if let Some(instance_ty) =
            self.infer_builtin_range_instance_type(callable_type, arguments, &call_arguments)
        {
            bindings = bindings.with_constructed_instance_type(db, instance_ty);
        }

        // basedpython: `float(...)` over literal arguments constructs a known float, and
        // `float("inf")` / `float("nan")` are python's only spelling of the special values
        if let Some(literal_ty) =
            self.infer_basedpython_float_literal_call(callable_type, arguments, &call_arguments)
        {
            bindings = bindings.with_constructed_instance_type(self.db(), literal_ty);
        }

        let db = self.db();
        let return_ty = bindings.return_type(db, env);
        let return_ty = match collection_initializer_class {
            Some(collection_class @ (KnownClass::List | KnownClass::Set))
                if return_ty
                    .class_specialization(db, env)
                    .is_some_and(|(class, _)| class.is_known(db, collection_class)) =>
            {
                self.infer_empty_list_or_set_constructor(
                    collection_class,
                    call_expression,
                    call_expression_tcx,
                )
                .unwrap_or(return_ty)
            }
            _ => return_ty,
        };

        self.check_narrowing_guard_as_value(call_expression, &bindings);

        self.record_unsolved_typevar_call(call_expression, return_ty, &bindings);

        typeguard::bind_type_guard_return_type(
            db,
            self.scope(),
            return_ty,
            &bindings,
            call_expression,
        )
    }

    /// basedpython: remember a call that only returns `Never` because it left a type variable
    /// unsolved, so that reachability analysis does not read it as a call that never returns.
    fn record_unsolved_typevar_call(
        &mut self,
        call_expression: &ast::ExprCall,
        return_ty: Type<'db>,
        bindings: &Bindings<'db>,
    ) {
        if return_ty.is_never() && bindings.returns_unsolved_typevar(self.db()) {
            self.unsolved_typevar_calls.insert(call_expression.into());
        }
    }

    /// basedpython: report a call to an assertion guard whose result is used as a value.
    ///
    /// An assertion guard narrows once it returns, so it only says anything when it is
    /// called as a statement. Its value is the `None` it returns.
    fn check_narrowing_guard_as_value(
        &self,
        call_expression: &ast::ExprCall,
        bindings: &Bindings<'db>,
    ) {
        if self.index.is_statement_call(call_expression) {
            return;
        }
        let asserts = bindings
            .single_element()
            .and_then(|binding| binding.matching_overloads().next())
            .is_some_and(|(_, overload)| {
                overload
                    .signature
                    .narrowing_guards
                    .iter()
                    .any(NarrowingGuard::is_assertion)
            });
        if !asserts {
            return;
        }
        if let Some(builder) = self
            .context
            .report_lint(&NARROWING_GUARD_AS_VALUE, call_expression)
        {
            builder.into_diagnostic(
                "an assertion guard narrows when it is called as a statement, \
                 and its value is only the `None` it returns",
            );
        }
    }

    fn infer_starred_expression(
        &mut self,
        starred: &ast::ExprStarred,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let env = self.program_environment();
        let ast::ExprStarred {
            range: _,
            node_index: _,
            value,
            ctx: _,
        } = starred;

        let db = self.db();
        let iterable_type = self.infer_expression(value, tcx);
        let typevartuple = match iterable_type {
            Type::KnownInstance(KnownInstanceType::TypeVar(typevar))
                if typevar.is_typevartuple(db) =>
            {
                bind_typevar(
                    self.db(),
                    self.index,
                    self.scope().file_scope_id(db),
                    self.typevar_binding_context,
                    typevar,
                )
            }
            Type::TypeVar(typevar) if typevar.is_typevartuple(db) => Some(typevar),
            _ => None,
        };
        if let Some(typevartuple) = typevartuple {
            return Type::tuple(TupleType::new(
                db,
                env,
                &TupleSpecBuilder::with_capacity(0)
                    .concat_variadic_typevar(db, env, typevartuple)
                    .build(),
            ));
        }

        report_iteration_over_character(&self.context, iterable_type, value.as_ref().into());
        iterable_type
            .try_iterate(db, env)
            .map(|spec| Type::tuple(TupleType::new(db, env, &spec)))
            .unwrap_or_else(|err| {
                err.report_diagnostic(&self.context, iterable_type, value.as_ref().into());
                Type::homogeneous_tuple(db, env, err.fallback_element_type(db, env))
            })
    }

    fn infer_yield_expression(&mut self, yield_expression: &ast::ExprYield) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let ast::ExprYield {
            range: _,
            node_index: _,
            value,
        } = yield_expression;
        let Some(enclosing_function) = nearest_enclosing_function(db, self.index, self.scope())
        else {
            let _ = self.infer_optional_expression(value.as_deref(), TypeContext::default());
            return Type::unknown();
        };
        let declared_return_ty = same_module_uncached_raw_signature(
            db,
            enclosing_function,
            ReturnCallableTypeVarScope::Public,
        )
        .return_ty;
        let return_type_span = enclosing_function.spans(self.db()).return_type;

        let Some(generator_type_params) = declared_return_ty.generator_types(db, env) else {
            let _ = self.infer_optional_expression(value.as_deref(), TypeContext::default());
            return Type::unknown();
        };

        let expected_yield_ty = generator_type_params.yield_ty;
        let tcx = TypeContext::new(expected_yield_ty);
        let yielded_ty = self
            .infer_optional_expression(value.as_deref(), tcx)
            .unwrap_or_else(|| Type::none(db, env));
        let diagnostic_node: AnyNodeRef = value
            .as_deref()
            .map_or_else(|| yield_expression.into(), AnyNodeRef::from);

        if let Some(expected_yield_ty) = expected_yield_ty {
            self.validate_generator_yield_type(
                diagnostic_node,
                YieldKind::Yield,
                return_type_span,
                expected_yield_ty,
                yielded_ty,
            );
        }

        generator_type_params.send_ty.unwrap_or_else(Type::unknown)
    }

    fn infer_yield_from_expression(&mut self, yield_from: &ast::ExprYieldFrom) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let ast::ExprYieldFrom {
            range: _,
            node_index: _,
            value,
        } = yield_from;

        let Some(enclosing_function) = nearest_enclosing_function(db, self.index, self.scope())
        else {
            let _ = self.infer_expression(value, TypeContext::default());
            return Type::unknown();
        };
        let annotated_return_ty = same_module_uncached_raw_signature(
            db,
            enclosing_function,
            ReturnCallableTypeVarScope::Public,
        )
        .return_ty;

        let Some(outer_expected) = annotated_return_ty.generator_types(db, env) else {
            let _ = self.infer_expression(value, TypeContext::default());
            return Type::unknown();
        };
        let return_type_span = enclosing_function.spans(self.db()).return_type;

        let tcx = TypeContext::new(outer_expected.yield_ty.map(|yielded_ty| {
            KnownClass::Iterable.to_specialized_instance(db, env, &[yielded_ty])
        }));
        let iterable_type = self.infer_expression(value, tcx);

        report_iteration_over_character(&self.context, iterable_type, value.as_ref().into());

        let known_inner_yield_type = match iterable_type.try_iterate(db, env) {
            Ok(tuple) => Some(tuple.homogeneous_element_type(db, env)),
            Err(err) => {
                err.report_diagnostic(&self.context, iterable_type, AnyNodeRef::from(&**value));
                err.element_type(db, env)
            }
        };

        if let Some(outer_yield_ty) = outer_expected.yield_ty
            && let Some(known_inner_yield_type) = known_inner_yield_type
        {
            self.validate_generator_yield_type(
                &**value,
                YieldKind::YieldFrom,
                return_type_span.clone(),
                outer_yield_ty,
                known_inner_yield_type,
            );
        }

        if let Some(outer_send_ty) = outer_expected.send_ty {
            let inner_send_ty = iterable_type
                .generator_send_type(db, env)
                .unwrap_or_else(|| Type::none(db, env));
            if !outer_send_ty.is_assignable_to(db, env, inner_send_ty) {
                report_invalid_generator_yield_type(
                    &self.context,
                    value.as_ref(),
                    return_type_span,
                    outer_send_ty,
                    inner_send_ty,
                    GeneratorMismatchKind::SendType,
                );
            }
        }

        iterable_type
            .generator_return_type(db, env)
            .unwrap_or_else(Type::unknown)
    }

    fn validate_generator_yield_type(
        &self,
        yielded_value: impl Ranged,
        yield_kind: YieldKind,
        return_type_span: Option<Span>,
        expected_yield_ty: Type<'db>,
        yielded_ty: Type<'db>,
    ) {
        let db = self.db();
        let env = self.program_environment();

        if !yielded_ty.is_assignable_to(db, env, expected_yield_ty) {
            report_invalid_generator_yield_type(
                &self.context,
                yielded_value,
                return_type_span,
                expected_yield_ty,
                yielded_ty,
                GeneratorMismatchKind::YieldType,
            );
        } else if self.context.is_lint_enabled(&UNSOUND_YIELD)
            && expected_yield_ty.is_fully_static(db, env)
            && !yielded_ty.is_pure_redundant_with(db, env, expected_yield_ty)
        {
            // N.B. the implementation here is the ~same as for `UNSOUND_RETURN_STATEMENT`;
            // update that too if updating this!
            report_unsound_yield(
                &self.context,
                yielded_value,
                yield_kind,
                return_type_span,
                expected_yield_ty,
                yielded_ty,
            );
        }
    }

    fn infer_await_expression(
        &mut self,
        await_expression: &ast::ExprAwait,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let ast::ExprAwait {
            range: _,
            node_index: _,
            value,
            postfix: _,
        } = await_expression;

        let expr_type = self.infer_expression(
            value,
            tcx.map(|tcx| KnownClass::Awaitable.to_specialized_instance(db, env, &[tcx])),
        );

        expr_type.try_await(db, env).unwrap_or_else(|err| {
            err.report_diagnostic(&self.context, expr_type, value.as_ref().into());
            Type::unknown()
        })
    }

    // Perform narrowing with applicable constraints between the current scope and the enclosing scope.
    fn narrow_place_with_applicable_constraints(
        &self,
        expr: PlaceExprRef,
        mut ty: Type<'db>,
        constraint_keys: &[(FileScopeId, ConstraintKey)],
    ) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        for (enclosing_scope_file_id, constraint_key) in constraint_keys {
            let use_def = self.index.use_def_map(*enclosing_scope_file_id);
            let place_table = self.index.place_table(*enclosing_scope_file_id);
            let place = place_table.place_id(expr).unwrap();

            match use_def.applicable_constraints(
                *constraint_key,
                *enclosing_scope_file_id,
                expr,
                self.index,
            ) {
                ApplicableConstraints::UnboundBinding(constraint) => {
                    ty = constraint.narrow(db, env, ty, place);
                }
                // Performs narrowing based on constrained bindings.
                // This handling must be performed even if narrowing is attempted and failed using `infer_place_load`.
                // The result of `infer_place_load` can be applied as is only when its boundness is `Bound`.
                // For example, this handling is required in the following case:
                // ```python
                // class C:
                //     x: int | None = None
                // c = C()
                // # c.x: int | None = <unbound>
                // if c.x is None:
                //     c.x = 1
                // # else: c.x: int = <unbound>
                // # `c.x` is not definitely bound here
                // reveal_type(c.x)  # revealed: int
                // ```
                ApplicableConstraints::ConstrainedBindings(bindings) => {
                    let reachability_constraints = bindings.reachability_constraints();
                    let predicates = bindings.predicates();
                    let mut union = UnionBuilder::new(db, env);
                    for binding in bindings {
                        let static_reachability = evaluate_reachability_with_cache(
                            db,
                            Some(self.reachability_cache()),
                            reachability_constraints,
                            predicates,
                            binding.reachability_constraint,
                        );
                        if static_reachability.is_always_false() {
                            continue;
                        }
                        match binding.binding {
                            DefinitionState::Defined(definition)
                                if !is_discarded_dict_key_assignment(db, definition) =>
                            {
                                let binding_ty = binding_type(db, definition);
                                union.add_in_place(
                                    binding
                                        .narrowing_constraint
                                        .narrow(db, env, binding_ty, place),
                                );
                            }
                            DefinitionState::Defined(_)
                            | DefinitionState::Undefined
                            | DefinitionState::Deleted => {
                                union.add_in_place(
                                    binding.narrowing_constraint.narrow(db, env, ty, place),
                                );
                            }
                        }
                    }
                    // If there are no visible bindings, the union becomes `Never`.
                    // Since an unbound binding is recorded even for an undefined place,
                    // this can only happen if the code is unreachable
                    // and therefore it is correct to set the result to `Never`.
                    ty = union.build();
                }
            }
        }
        ty
    }

    /// Check if the given ty is `@deprecated` or not
    fn check_deprecated<T: Ranged>(&self, ranged: T, ty: Type) {
        // First handle classes
        if let Type::ClassLiteral(class_literal) = ty {
            let Some(deprecated) = class_literal.deprecated(self.db()) else {
                return;
            };

            let Some(builder) = self.context.report_lint(&diagnostic::DEPRECATED, ranged) else {
                return;
            };

            let class_name = class_literal.name(self.db());
            let mut diag =
                builder.into_diagnostic(format_args!(r#"The class `{class_name}` is deprecated"#));
            if let Some(message) = deprecated.message {
                diag.set_primary_annotation_message(message.value(self.db()));
            }
            diag.add_primary_tag(ruff_db::diagnostic::DiagnosticTag::Deprecated);
            return;
        }

        // Next handle functions
        let function = match ty {
            Type::FunctionLiteral(function) => function,
            Type::BoundMethod(bound) => bound.function(self.db()),
            _ => return,
        };

        // Currently we only check the final implementation for deprecation, as
        // that check can be done on any reference to the function. Analysis of
        // deprecated overloads needs to be done in places where we resolve the
        // actual overloads being used.
        let Some(deprecated) = function.implementation_deprecated(self.db()) else {
            return;
        };

        let Some(builder) = self
            .context
            .report_lint(&crate::types::diagnostic::DEPRECATED, ranged)
        else {
            return;
        };

        let func_name = function.name(self.db());
        let mut diag =
            builder.into_diagnostic(format_args!(r#"The function `{func_name}` is deprecated"#));
        if let Some(message) = deprecated.message {
            diag.set_primary_annotation_message(message.value(self.db()));
        }
        diag.add_primary_tag(ruff_db::diagnostic::DiagnosticTag::Deprecated);
    }

    /// basedpython: whether `name_node` reads the `it` a trailing-lambda block binds when
    /// the block's callback passes no argument for it.
    ///
    /// The lowering always gives the block that parameter — `def _trailing_lambda_0(it=None)`
    /// — so the name always resolves and always shadows an outer `it`, exactly as it does at
    /// runtime. When the callback is invoked as `fn()` nothing ever fills it, and the body is
    /// reading the default rather than a value the call passed.
    ///
    /// A nested binding takes the name back — a comprehension target, an assignment in the
    /// body — and reading *that* is ordinary code, so only a read that reaches the parameter
    /// itself is reported.
    fn reads_unfilled_block_it(&self, name_node: &ast::ExprName) -> bool {
        let db = self.db();
        if !self.is_basedpython_file() {
            return false;
        }
        let Some((block_scope, callee)) = enclosing_block(db, self.scope()) else {
            return false;
        };
        if trailing_lambda_passes_it(db, callee) != Some(false) {
            return false;
        }
        let block_file_scope = block_scope.file_scope_id(db);
        let module = self.module();
        let Some(block) = block_scope.scope(db).node().as_function() else {
            return false;
        };
        let Some(it) = block.node(module).parameters.args.first() else {
            return false;
        };
        if it.parameter.name.id != name_node.id {
            return false;
        }
        let Some(parameter) = self.index.try_definition(&it.parameter) else {
            return false;
        };
        let file_scope = self.scope().file_scope_id(db);
        if file_scope != block_file_scope {
            // a comprehension opened inside the block: the parameter is reached through the
            // enclosing scope, so it is what this reads unless something here claims the name
            return self
                .index
                .place_table(file_scope)
                .symbol_id(&name_node.id)
                .is_none();
        }
        let use_id = ast::ExprRef::Name(name_node).scoped_use_id(db, self.program_file());
        self.index
            .use_def_map(file_scope)
            .bindings_at_use(use_id)
            .any(|binding| binding.binding.definition() == Some(parameter))
    }

    fn infer_name_load(&mut self, name_node: &ast::ExprName, tcx: TypeContext<'db>) -> Type<'db> {
        let symbol_name = &name_node.id;
        let db = self.db();
        let env = self.program_environment();

        // basedpython only: inside a trailing lambda block whose callback
        // declares a receiver (`int.() -> None`), the receiver is spelled `self`
        // and its members are in scope unqualified. the receiver sits in the
        // scope tower at the block's own level, so it is resolved *before* the
        // ordinary lookup — only a name the block itself binds keeps its meaning
        if self.is_basedpython_file()
            && let Some(resolved) = receivers::implicit_receiver_name(
                db,
                env,
                self.file(),
                self.scope(),
                symbol_name,
                Some(name_node),
            )
        {
            return resolved.ty();
        }

        // basedpython: the block always has an `it` parameter, because the lambda the
        // lowering writes always declares one — but a callback invoked as `fn()` never
        // fills it, so reading it reads the `None` default rather than a value
        if self.reads_unfilled_block_it(name_node)
            && let Some(builder) = self
                .context
                .report_lint(&TRAILING_LAMBDA_PARAMETERS, name_node)
        {
            let mut diagnostic = builder.into_diagnostic(
                "this block's callback passes no argument, so `it` is never given a value",
            );
            diagnostic
                .info("the block is called as `fn()`, which leaves `it` at its `None` default");
        }

        let expr = PlaceExpr::from_expr_name(name_node);

        let (resolved, _) = self.infer_place_load(expr, ast::ExprRef::Name(name_node));

        let resolved_after_fallback = resolved
            // basedpython only: the names that mean a module member the file
            // never imported — `Mapping`, `Character`, the `dynamic` spelling of
            // `Any`, the return-value markers. the transpiler lowers each of
            // them; `implicit_names` is what both it and this resolution read,
            // including which kind of position each one means its member in.
            // only reached when the name is otherwise unbound, so a local
            // `Character = …` binding still shadows it
            .or_fall_back_to(db, env, || {
                if self.is_basedpython_file()
                    && let Some(implicit) = implicit_name(symbol_name)
                    && implicit.position.admits(
                        self.inference_flags()
                            .contains(InferenceFlags::IN_TYPE_EXPRESSION),
                    )
                {
                    implicit.resolve(db, env)
                } else {
                    Place::Undefined.into()
                }
            })
            // basedpython only: `Some` is the present-case optional constructor.
            // It has no runtime definition in real Python — the transpiler lowers
            // `Some(x)` to the injected `Optional(x)` wrapper — so it's resolved
            // magically here rather than via a typeshed stub. Only reached when
            // otherwise unbound, so a local `Some = …` binding still shadows it.
            // It takes exactly one value (so `Some()` / `Some(1, 2)` are arity
            // errors) and produces the wrapped optional of that value's type
            .or_fall_back_to(db, env, || {
                if self.is_basedpython_file() && symbol_name == "Some" {
                    let value_typevar = BoundTypeVarInstance::synthetic(
                        db,
                        env,
                        Name::new_static("_SomeT"),
                        TypeVarVariance::Covariant,
                    );
                    let value_ty = Type::TypeVar(value_typevar);
                    let value = Parameter::positional_only(Some(Name::new_static("value")))
                        .with_annotated_type(value_ty);
                    let signature = Signature::new_generic(
                        Some(GenericContext::from_typevar_instances(
                            db,
                            env,
                            [value_typevar],
                        )),
                        Parameters::from_annotation(db, env, [value]),
                        Type::KnownInstance(KnownInstanceType::WrappedOptional(InternedType::new(
                            db, value_ty,
                        ))),
                    );
                    Place::bound(Type::single_callable(db, signature)).into()
                } else {
                    Place::Undefined.into()
                }
            })
            // basedpython only: inside an `extension` body, the extended
            // type's own type parameters are in scope under the names its
            // declaration bound (`Element` on `list`). bracket-spelled
            // (constrained) params resolve normally through the type-param
            // scope before this fallback is reached. the name resolves to the
            // typevar *object* — `bind_typevar` recognises extension bodies as
            // binding the extended class's parameters, exactly as a class body
            // binds its own
            .or_fall_back_to(db, env, || {
                if self.is_basedpython_file()
                    && let Some(extension) = self.enclosing_extension()
                    && let Some(typevar) =
                        extensions::extension_body_typevar(db, extension, symbol_name)
                {
                    Place::bound(Type::KnownInstance(KnownInstanceType::TypeVar(
                        typevar.typevar(db),
                    )))
                    .into()
                } else {
                    Place::Undefined.into()
                }
            })
            // basedpython only: context-sensitive resolution. where the
            // expression's expected type is known, a name that resolves to
            // nothing else is looked up as a member of that type — `Red` in a
            // `Color` context means `Color.Red`. reached last of all, so it is
            // purely additive: nothing that resolves today changes meaning
            .or_fall_back_to(db, env, || {
                if self.is_basedpython_file()
                    && let Some(member) = context_sensitive::resolve_in_context(
                        db,
                        env,
                        self.file(),
                        self.scope(),
                        tcx,
                        symbol_name,
                    )
                {
                    Place::bound(member.ty).into()
                } else {
                    Place::Undefined.into()
                }
            })
            // basedpython: the same rule for the class of a case pattern
            // (`case Circle(r):`), whose expected type is the subject's rather
            // than anything the surrounding expression can carry. Resolved from
            // the name itself, not from a `tcx`, so that every reader of this
            // expression's type — the narrowing and exhaustiveness analyses
            // included — gets the one answer
            .or_fall_back_to(db, env, || {
                if self.is_basedpython_file()
                    && let Some(case_name) = self.index.case_name(NodeKey::from_node(name_node))
                    && let Some(member) = context_sensitive::resolve_case_name(db, env, case_name)
                {
                    Place::bound(member.ty).into()
                } else {
                    Place::Undefined.into()
                }
            });

        let ty = resolved_after_fallback.unwrap_with_diagnostic(db, env, |lookup_error| {
            match lookup_error {
                LookupError::Undefined(qualifiers) => {
                    self.report_unresolved_reference(name_node, tcx);
                    TypeAndQualifiers::new(Type::unknown(), TypeOrigin::Inferred, qualifiers)
                }
                LookupError::PossiblyUndefined(type_when_bound) => {
                    if let Some(mut diagnostic) =
                        report_possibly_unresolved_reference(&self.context, name_node)
                        && let Some(declaration) = self.block_scoped_declaration_for(name_node)
                    {
                        self.explain_block_scope(&mut diagnostic, name_node, declaration);
                    }
                    type_when_bound
                }
            }
        });

        let ty = ty.inner_type();

        // basedpython: a pep 695 function type parameter referenced in a value
        // position is reified — the runtime value is the supplied type
        // argument, so the reference types as `type[T]` rather than as the
        // `TypeVar` object. a `*Ts` parameter absorbs a whole run of type
        // arguments, so its value is a tuple of them; a `**Kwargs` pack binds
        // its fields, so its value is a mapping of field name to type
        if self.is_basedpython_file()
            && !self
                .inference_flags()
                .contains(InferenceFlags::IN_TYPE_EXPRESSION)
            && let Type::KnownInstance(KnownInstanceType::TypeVar(typevar)) = ty
            && matches!(
                typevar.kind(db),
                TypeVarKind::Pep695TypeVar
                    | TypeVarKind::Pep695TypeVarTuple
                    | TypeVarKind::Pep695KeywordVariadic
            )
            && typevar.definition(db).is_some_and(|definition| {
                matches!(
                    definition.scope(db).node(db),
                    NodeWithScopeKind::FunctionTypeParameters(_)
                )
            })
            && let Some(bound_typevar) = bind_typevar(
                db,
                self.index,
                self.scope().file_scope_id(db),
                self.typevar_binding_context,
                typevar,
            )
        {
            let kind = typevar.kind(db);
            if kind.is_typevartuple() {
                return Type::homogeneous_tuple(db, env, KnownClass::Type.to_instance(db, env));
            }
            if kind.is_keyword_variadic() {
                return KnownClass::Dict.to_specialized_instance(
                    db,
                    env,
                    &[
                        KnownClass::Str.to_instance(db, env),
                        KnownClass::Type.to_instance(db, env),
                    ],
                );
            }
            return Type::TypeVar(bound_typevar).to_meta_type(db, env);
        }

        ty
    }

    /// Infer the type of a place expression from its ordered load sources.
    ///
    /// This also returns the [`ConstraintKey`]s used by expression-level narrowing.
    fn infer_place_load(
        &self,
        place_expr: PlaceExpr,
        expr_ref: ast::ExprRef,
    ) -> (PlaceAndQualifiers<'db>, Vec<(FileScopeId, ConstraintKey)>) {
        let env = self.program_environment();
        let mode = if self.is_deferred() && self.in_string_annotation() {
            PlaceLoadMode::StringAnnotation
        } else if self.is_deferred() {
            PlaceLoadMode::Deferred
        } else {
            PlaceLoadMode::AtExpression(expr_ref)
        };
        let mut resolution =
            resolve_place_load(self.db(), self.index, self.scope(), place_expr, mode);
        let mut place = PlaceAndQualifiers::from(Place::Undefined);
        let mut failure = None;
        let mut checked_deprecated = false;

        while let Some(step) = resolution.next() {
            match step {
                PlaceLoadResolutionStep::Source(source) => {
                    if !checked_deprecated && source.is_post_lexical() {
                        // Deprecation diagnostics apply to the result of lexical name resolution,
                        // before it is combined with implicit module globals or builtins. Hence, we
                        // check for deprecation here when the first post-lexical source is yielded.
                        // If resolution stops before this, then the check after the resolution loop
                        // handles the final lexical result instead.
                        if let Some(ty) = place.place.ignore_possibly_undefined() {
                            self.check_deprecated(expr_ref, ty);
                        }
                        checked_deprecated = true;
                    }
                    let narrowing_constraints = resolution.narrowing_constraints_for(&source);
                    place = place.or_fall_back_to(self.db(), env, || {
                        self.infer_place_load_source(
                            resolution.place_expr(),
                            source,
                            narrowing_constraints,
                        )
                    });
                    if place.place.is_definitely_bound() {
                        break;
                    }
                }
                PlaceLoadResolutionStep::MemberResolutionCondition(prefix_loads) => {
                    if self.has_bound_place_expr_prefix(&prefix_loads) {
                        failure = Some(PlaceLoadFailure::NotFound);
                        break;
                    }
                }
                PlaceLoadResolutionStep::Exhausted(exhaustion_failure) => {
                    failure = Some(exhaustion_failure);
                    break;
                }
            }
        }

        if !checked_deprecated && let Some(ty) = place.place.ignore_possibly_undefined() {
            self.check_deprecated(expr_ref, ty);
        }

        let place = if failure == Some(PlaceLoadFailure::NotFound) {
            place.or_fall_back_to(self.db(), env, || {
                self.infer_unimported_reveal_type_fallback(expr_ref)
            })
        } else {
            place
        };

        let constraint_keys = resolution.into_constraints();

        (place, constraint_keys)
    }

    fn infer_place_load_source(
        &self,
        place_expr: PlaceExprRef,
        source: PlaceLoadSource<'db>,
        narrowing_constraints: &[(FileScopeId, ConstraintKey)],
    ) -> PlaceAndQualifiers<'db> {
        let db = self.db();
        let env = self.program_environment();
        let is_class_body_global_fallback = source.is_class_body_global_fallback();

        let place = match source.kind {
            PlaceLoadSourceKind::Bindings(bindings) => {
                let mut place = place_from_bindings_with_reachability_cache(
                    db,
                    env,
                    bindings,
                    self.reachability_cache(),
                )
                .place;

                // Compatibility policy: ty historically treats a possibly-bound module snapshot
                // reached through a class-body global fallback as definitely bound. At runtime,
                // an unbound snapshot would continue to builtins or produce a name error.
                if is_class_body_global_fallback && let Place::Defined(defined) = place {
                    place = Place::Defined(defined.with_definedness(Definedness::AlwaysDefined));
                }

                place.into()
            }
            // definitely bound: the debugger read a value out of the frame, so there is no
            // question of the name being unbound at this point. `DefinedPlace::new` is already
            // `AlwaysDefined`
            PlaceLoadSourceKind::Observed(ty) => Place::Defined(DefinedPlace::new(ty)).into(),
            PlaceLoadSourceKind::DefinitionsFromOwningScope { scope, id } => place_by_id(
                db,
                scope,
                id,
                RequiresExplicitReExport::No,
                ConsideredDefinitions::AllReachable,
            ),
            PlaceLoadSourceKind::Implicit(implicit) => match implicit {
                ImplicitPlaceLoad::DunderClass(definition) => original_class_type(db, definition)
                    .map_or_else(
                        || Place::Undefined.into(),
                        |class| Place::bound(class).into(),
                    ),
                ImplicitPlaceLoad::ClassBodySymbol(name) => {
                    let implicit = class_body_implicit_symbol(db, env, &name);
                    if implicit.place.is_definitely_bound() {
                        implicit
                    } else {
                        Place::Undefined.into()
                    }
                }
                ImplicitPlaceLoad::ExplicitGlobalSymbol { file, name } => {
                    explicit_global_symbol(db, file, &name)
                }
                ImplicitPlaceLoad::ModuleImplicitGlobal { file, name } => {
                    module_type_implicit_global_symbol(db, file, &name)
                }
                ImplicitPlaceLoad::Builtin(name) => {
                    if Some(self.scope()) == builtins_module_scope(db, env) {
                        Place::Undefined.into()
                    } else {
                        implicit_builtins_symbol(db, env, &name)
                    }
                }
            },
        };

        if narrowing_constraints.is_empty() {
            place
        } else {
            place.map_type(|ty| {
                self.narrow_place_with_applicable_constraints(place_expr, ty, narrowing_constraints)
            })
        }
    }

    /// Applies ty's convenience fallback for an unimported `reveal_type`.
    fn infer_unimported_reveal_type_fallback(
        &self,
        expr_ref: ast::ExprRef,
    ) -> PlaceAndQualifiers<'db> {
        let Some(name) = expr_ref
            .as_name_expr()
            .filter(|name| name.id == "reveal_type")
        else {
            return Place::Undefined.into();
        };

        if !self.in_stub()
            && !self.is_in_type_checking_block(self.scope(), name)
            && let Some(builder) = self.context.report_lint(&UNDEFINED_REVEAL, name)
        {
            let mut diag = builder.into_diagnostic("`reveal_type` used without importing it");
            diag.info("This is allowed for debugging convenience but will fail at runtime");
        }

        typing_extensions_symbol(self.db(), self.program_environment(), "reveal_type")
    }

    /// Returns whether any tracked place-expression prefix has a definite or possible binding in
    /// this scope.
    fn has_bound_place_expr_prefix(&self, prefix_loads: &PlaceExprPrefixLoads<'db>) -> bool {
        let db = self.db();
        let env = self.program_environment();
        let file_scope_id = prefix_loads.scope().file_scope_id(db);
        let use_def = self.index.use_def_map(file_scope_id);

        prefix_loads.iter().any(|prefix| {
            let place = match prefix {
                PlaceExprPrefixLoad::AtUse(use_id) => {
                    place_from_bindings_with_reachability_cache(
                        db,
                        env,
                        use_def.bindings_at_use(use_id),
                        self.reachability_cache(),
                    )
                    .place
                }
                PlaceExprPrefixLoad::AllReachable(place_id) => {
                    place_from_bindings_with_reachability_cache(
                        db,
                        env,
                        use_def.reachable_bindings(place_id),
                        self.reachability_cache(),
                    )
                    .place
                }
                PlaceExprPrefixLoad::DefinitelyBound => return true,
            };

            !place.is_undefined()
        })
    }

    /// basedpython: the block-scoped `let` / `var` declaration that put `name` out
    /// of scope, if that is why it resolves to nothing here.
    ///
    /// A declaration written inside a block is unbound when the block ends, so a
    /// use after it looks exactly like a use of a name that was never declared.
    /// This is what separates the two.
    fn block_scoped_declaration_for(
        &self,
        name: &ast::ExprName,
    ) -> Option<&'db BlockScopedDeclaration> {
        if !self.is_basedpython_file() {
            return None;
        }
        let scope = self.scope().file_scope_id(self.db());
        let symbol = self.index.place_table(scope).symbol_id(&name.id)?;
        let declaration = self
            .index
            .block_scoped_declaration(scope, symbol, name.start())?;
        // a use *inside* the block is in scope; only one after it is not
        (!declaration.block.contains_range(name.range())).then_some(declaration)
    }

    /// basedpython: says that a name resolves to nothing here because the block its
    /// declaration was written in has ended.
    fn explain_block_scope(
        &self,
        diagnostic: &mut Diagnostic,
        name: &ast::ExprName,
        declaration: &BlockScopedDeclaration,
    ) {
        let keyword = match declaration.keyword {
            BindingKeyword::Let => "let",
            BindingKeyword::Var => "var",
        };
        diagnostic.info(format_args!(
            "`{id}` is declared with `{keyword}`, so it is in scope only inside the block \
             that declares it",
            id = name.id
        ));
        diagnostic.annotate(
            Annotation::secondary(Span::from(self.file()).with_range(declaration.keyword_range))
                .message(format_args!("`{id}` is declared here", id = name.id)),
        );
    }

    pub(super) fn report_unresolved_reference(
        &self,
        expr_name_node: &ast::ExprName,
        tcx: TypeContext<'db>,
    ) {
        let db = self.db();
        let env = self.program_environment();
        let Some(builder) = self
            .context
            .report_lint(&UNRESOLVED_REFERENCE, expr_name_node)
        else {
            return;
        };

        let ast::ExprName { id, .. } = expr_name_node;
        let mut diagnostic =
            builder.into_diagnostic(format_args!("Name `{id}` used when not defined"));

        // ===
        // Subdiagnostic (-1), basedpython only: the name *was* declared, in a block
        // that has since ended, so it is out of scope rather than unknown
        // ===
        if let Some(declaration) = self.block_scoped_declaration_for(expr_name_node) {
            self.explain_block_scope(&mut diagnostic, expr_name_node, declaration);
            return;
        }

        // ===
        // Subdiagnostic (0), basedpython only: the expected type declares a member
        // of this name, but one of the context-sensitive resolution rules kept it
        // from answering. Say which, since the qualified spelling always works
        // ===
        if self.is_basedpython_file()
            && let Some(miss) =
                context_sensitive::explain_miss(self.db(), env, self.file(), self.scope(), tcx, id)
        {
            let db = self.db();
            match miss {
                context_sensitive::Miss::Shadowed(enum_class) => diagnostic.info(format_args!(
                    "`{enum_name}` declares `{id}`, but this scope binds `{id}` itself: \
                     write `{enum_name}.{id}`",
                    enum_name = enum_class.name(db)
                )),
                context_sensitive::Miss::Unnameable(enum_class) => diagnostic.info(format_args!(
                    "`{id}` is a member of `{enum_name}`, which is not in scope here \
                     under that name",
                    enum_name = enum_class.name(db)
                )),
                context_sensitive::Miss::Ambiguous(first, second) => diagnostic.info(format_args!(
                    "`{first_name}` and `{second_name}` both declare `{id}`: \
                     write it qualified",
                    first_name = first.name(db),
                    second_name = second.name(db)
                )),
            }
        }

        // ===
        // Subdiagnostic (1): check to see if it was added as a builtin in a later version of Python.
        // ===
        if let Some(version_added_to_builtins) = version_builtin_was_added(id) {
            diagnostic.info(format_args!(
                "`{id}` was added as a builtin in Python 3.{version_added_to_builtins}"
            ));
            add_inferred_python_version_hint_to_diagnostic(
                db,
                self.file(),
                &mut diagnostic,
                "resolving types",
            );
        }

        // ===
        // Subdiagnostic (2): check to see if it's a capitalized older type hint that is available as lowercase in this version of Python.
        // ===
        // We don't need to check for typing_extensions.Type,
        // because it's already caught by typing.Type.
        if self.program_environment().python_version(db) >= PythonVersion::PY39 {
            if let Some(("", builtin_name)) = as_pep_585_generic("typing", id) {
                diagnostic
                    .set_primary_annotation_message(format_args!("Did you mean `{builtin_name}`?"));
            }
        }

        // ===
        // Subdiagnostic (3):
        // - If it's an instance method, check to see if it's available as an attribute on `self`;
        // - If it's a classmethod, check to see if it's available as an attribute on `cls`
        // ===
        let Some(current_function) = self.current_function_definition() else {
            return;
        };

        let function_parameters = &*current_function.parameters;

        // `self`/`cls` can't be a keyword-only parameter.
        if function_parameters.posonlyargs.is_empty() && function_parameters.args.is_empty() {
            return;
        }

        let Some(first_parameter) = function_parameters.iter_non_variadic_params().next() else {
            return;
        };

        let Some(class) = self.class_context_of_current_method() else {
            return;
        };

        let first_parameter_name = first_parameter.name();

        let Some(function_type) = self.current_function_type() else {
            return;
        };

        let attribute_exists = match MethodDecorator::try_from_fn_type(self.db(), function_type) {
            Some(MethodDecorator::ClassMethod) => !Type::instance(db, env, class)
                .class_member(db, env, id)
                .place
                .is_undefined(),
            Some(MethodDecorator::None) => !Type::instance(db, env, class)
                .member(db, env, id)
                .place
                .is_undefined(),
            Some(MethodDecorator::StaticMethod) | None => false,
        };

        if attribute_exists {
            diagnostic.info(format_args!(
                "An attribute `{id}` is available: consider using `{first_parameter_name}.{id}`"
            ));
        }
    }

    fn infer_name_expression(&mut self, name: &ast::ExprName, tcx: TypeContext<'db>) -> Type<'db> {
        match name.ctx {
            ExprContext::Load => {
                let ty = self.infer_name_load(name, tcx);
                // basedpython: a `type def` has no runtime existence — the
                // declaration is erased when transpiling — so naming one in a value
                // position would emit python that raises `NameError`
                if ty.is_type_fn(self.db())
                    && !self
                        .context
                        .inference_flags
                        .contains(InferenceFlags::IN_TYPE_EXPRESSION)
                    && let Some(builder) = self
                        .context
                        .report_lint(&crate::types::diagnostic::INVALID_TYPE_FORM, name)
                {
                    builder.into_diagnostic(format_args!(
                        "`{}` is a `type def`; it can only be applied in a type \
                         expression, not used as a value",
                        name.id
                    ));
                    return Type::unknown();
                }
                ty
            }
            ExprContext::Store => Type::Never,
            ExprContext::Del => {
                self.infer_name_load(name, TypeContext::default());
                Type::Never
            }
            ExprContext::Invalid => Type::unknown(),
        }
    }

    /// basedpython: a bare assignment inside a trailing lambda block writes the
    /// block receiver's member of that name, so it is checked against that
    /// member's declared type rather than binding a name of its own.
    ///
    /// TODO: route this through `validate_attribute_assignment`, which is what
    /// `self.href = …` goes through, so a write to a read-only property or through
    /// `__setattr__` is judged the same way. That validator takes the written
    /// `ExprAttribute` — for its diagnostic ranges, and for the object expression
    /// it hands to `__setattr__` — and a block assignment has neither, since the
    /// receiver is a parameter the source cannot spell. Generalizing its target to
    /// cover both spellings is the remaining piece.
    fn validate_receiver_member_write(&mut self, name: &ast::ExprName, value: &ast::Expr) {
        if !self.is_basedpython_file() {
            return;
        }
        let db = self.db();
        let env = self.program_environment();
        let Some(receivers::ImplicitReceiverName::Member(member_ty)) =
            receivers::implicit_receiver_name(db, env, self.file(), self.scope(), &name.id, None)
        else {
            return;
        };
        let Some(value_ty) = self.try_expression_type(value) else {
            return;
        };
        if value_ty.is_assignable_to(db, env, member_ty) {
            return;
        }
        if let Some(builder) = self
            .context
            .report_lint(&crate::types::diagnostic::INVALID_ASSIGNMENT, name)
        {
            builder.into_diagnostic(format_args!(
                "Object of type `{}` is not assignable to attribute `{}` of type `{}`",
                value_ty.display(db, env),
                name.id,
                member_ty.display(db, env),
            ));
        }
    }

    /// basedpython: a declaration inside a trailing lambda block takes its name
    /// away from the receiver's member of that name, for the whole block. A bare
    /// assignment writes the member instead, so the two forms mean opposite
    /// things and the one that shadows says so.
    fn report_shadowed_receiver_member(&mut self, name: &ast::ExprName) {
        if !self.is_basedpython_file() {
            return;
        }
        let db = self.db();
        let env = self.program_environment();
        let Some(member) = receivers::shadowed_receiver_member(db, env, self.scope(), &name.id)
        else {
            return;
        };
        if let Some(builder) = self
            .context
            .report_lint(&crate::types::diagnostic::SHADOWED_RECEIVER_MEMBER, name)
        {
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "Declaring `{}` shadows the receiver's `{}` for the whole block",
                name.id, name.id
            ));
            diagnostic.info(format_args!(
                "the receiver's `{}` is of type `{}`",
                name.id,
                member.display(db, env)
            ));
            diagnostic.help(format_args!(
                "Rename the declaration, or drop it to write `{}` on the receiver",
                name.id
            ));
        }
    }

    fn narrow_expr_with_applicable_constraints<'r>(
        &mut self,
        target: impl Into<ast::ExprRef<'r>>,
        target_ty: Type<'db>,
        constraint_keys: &[(FileScopeId, ConstraintKey)],
    ) -> Type<'db> {
        let target = target.into();

        if let Some(place_expr) = PlaceExpr::try_from_expr(target) {
            self.narrow_place_with_applicable_constraints(
                PlaceExprRef::from(&place_expr),
                target_ty,
                constraint_keys,
            )
        } else {
            target_ty
        }
    }

    /// basedpython: compute the bound-super type for `super` / `super[T]`
    /// when used as an attribute base. returns `None` when `value` does not
    /// match the sugar form, when there is no enclosing class, or when the
    /// supplied target class is not in the enclosing class' MRO.
    fn basedpython_super_value_type(&mut self, value: &ast::Expr) -> Option<Type<'db>> {
        let env = self.program_environment();
        let db = self.db();

        let target_class: Option<ClassType<'db>> = match value {
            ast::Expr::Name(n) if n.id.as_str() == "super" => None,
            ast::Expr::Subscript(s) => {
                let ast::Expr::Name(n) = s.value.as_ref() else {
                    return None;
                };
                if n.id.as_str() != "super" {
                    return None;
                }
                Some(self.expression_type(s.slice.as_ref()).to_class_type(db)?)
            }
            _ => return None,
        };

        // `super` needs the implicit arguments a method supplies — the `__class__`
        // cell and the receiver. directly in a class body there is neither, so
        // `super().x` there raises at runtime; report it rather than lower it
        let reaches_a_function = self
            .index
            .ancestor_scopes(self.scope().file_scope_id(db))
            .take_while(|(_, ancestor)| ancestor.node().as_class().is_none())
            .any(|(_, ancestor)| ancestor.node().as_function().is_some());
        // both unavailable cases answer `Unknown` rather than declining: falling
        // through would let the ordinary attribute load report a second, less
        // useful `Class `super` has no attribute …` on top of this one
        if !reaches_a_function {
            if let Some(builder) = self
                .context
                .report_lint(&UNAVAILABLE_IMPLICIT_SUPER_ARGUMENTS, value)
            {
                builder.into_diagnostic(
                    "Cannot determine implicit arguments for `super` outside a method",
                );
            }
            return Some(Type::unknown());
        }

        let Some(enclosing) = nearest_enclosing_class(db, self.index, self.scope()) else {
            // in a function, but no class encloses it — `super` has a `__class__`
            // cell to read only inside a method, so this is the same failure
            if let Some(builder) = self
                .context
                .report_lint(&UNAVAILABLE_IMPLICIT_SUPER_ARGUMENTS, value)
            {
                builder.into_diagnostic(
                    "Cannot determine implicit arguments for `super`: \
                     no class encloses this function",
                );
            }
            return Some(Type::unknown());
        };
        let enclosing_class = ClassLiteral::Static(enclosing).default_specialization(db);
        let owner_type = Type::instance(db, env, enclosing_class);

        let pivot_class_type = match target_class {
            None => Type::from(enclosing_class),
            Some(target) => {
                let target_lit = target.class_literal(db);
                let mut prev: Option<ClassType<'db>> = None;
                let mut found: Option<ClassType<'db>> = None;
                for entry in enclosing_class.class_literal(db).iter_mro(db) {
                    if let crate::types::ClassBase::Class(c) = entry {
                        if c.class_literal(db) == target_lit {
                            found = prev;
                            break;
                        }
                        prev = Some(c);
                    }
                }
                Type::from(found?)
            }
        };

        crate::types::bound_super::BoundSuperType::build(db, env, pivot_class_type, owner_type).ok()
    }

    /// Resolve `receiver` to the type the next link of a basedpython optional chain must be
    /// looked up against, and report whether that chain can short-circuit.
    ///
    /// `receiver_type` is the receiver's own type, which already carries any short-circuit
    /// `None`. See [`Self::basedpython_chain_present`].
    fn basedpython_chain_receiver(
        &self,
        receiver: &ast::Expr,
        receiver_type: Type<'db>,
    ) -> (Type<'db>, bool) {
        // every attribute, call and subscript asks; only a `.by` file that has already walked
        // a `?.` can answer
        if self.basedpython_chain_present.is_empty() {
            return (receiver_type, false);
        }
        match self
            .basedpython_chain_present
            .get(&ExpressionNodeKey::from(receiver))
        {
            Some(present) => (*present, true),
            None => (receiver_type, false),
        }
    }

    /// Record a basedpython optional-chain link's present-receiver type and return the type
    /// the link itself evaluates to.
    ///
    /// `short_circuits` is whether a `?.` anywhere in the chain can be absent; when it is,
    /// the link evaluates to `present | None` and the chain carries on from `present`.
    fn basedpython_chain_result(
        &mut self,
        link: impl Into<ExpressionNodeKey>,
        present: Type<'db>,
        short_circuits: bool,
    ) -> Type<'db> {
        let env = self.program_environment();
        if !short_circuits {
            return present;
        }
        self.basedpython_chain_present.insert(link.into(), present);
        // the present type comes first so the chain reads the way the field was declared
        // (`str | None`), rather than leading with the arm the chain only takes when it
        // short-circuits
        UnionType::from_two_elements(self.db(), env, present, Type::none(self.db(), env))
    }

    /// Infer an attribute load, returning its recovery type if lookup fails.
    fn infer_attribute_load(
        &mut self,
        attribute: &ast::ExprAttribute,
    ) -> Result<Type<'db>, Type<'db>> {
        let value_type =
            self.infer_maybe_standalone_expression(&attribute.value, TypeContext::default());
        let (value_type, in_chain) = self.basedpython_chain_receiver(&attribute.value, value_type);
        self.infer_attribute_load_chained(attribute, value_type, in_chain)
    }

    /// Infer an attribute load on a known receiver that does not continue a basedpython
    /// optional chain, returning its recovery type if lookup fails.
    fn infer_attribute_load_impl(
        &mut self,
        attribute: &ast::ExprAttribute,
        value_type: Type<'db>,
    ) -> Result<Type<'db>, Type<'db>> {
        self.infer_attribute_load_chained(attribute, value_type, false)
    }

    /// Infer the type of a [`ast::ExprAttribute`] expression, assuming a load context.
    ///
    /// `in_chain` is whether `value_type` came from an already-short-circuiting basedpython
    /// optional chain, which this access extends.
    fn infer_attribute_load_chained(
        &mut self,
        attribute: &ast::ExprAttribute,
        mut value_type: Type<'db>,
        in_chain: bool,
    ) -> Result<Type<'db>, Type<'db>> {
        fn union_elements_missing_attribute<'db>(
            db: &'db dyn Db,
            env: &ProgramEnvironment<'db>,
            ty: Type<'db>,
            attr_name: &str,
            missing_types: &mut FxIndexSet<Type<'db>>,
        ) {
            if let Some(union) = ty.as_union_like(db) {
                for element in union.elements(db) {
                    union_elements_missing_attribute(db, env, *element, attr_name, missing_types);
                }
            } else if ty.member(db, env, attr_name).place.is_undefined() {
                missing_types.insert(ty);
            }
        }

        let env = self.program_environment();
        let ast::ExprAttribute { value, attr, .. } = attribute;

        let db = self.db();
        let mut constraint_keys = vec![];

        // basedpython `a?.b`: short-circuits to `None` when `a is None`.
        // narrow value_type to its non-None component for the attribute
        // lookup, then re-union with None at the end. records whether we
        // performed the narrowing so we know to re-add None
        let mut none_chain_was_optional = in_chain;
        if self.is_basedpython_file() && attribute.optional {
            // a wrapped optional's present value is its inner type — peel the
            // wrapper before the lookup (the runtime reads `.value`); the
            // wrapper's absent outer state short-circuits to `None`
            if let Type::KnownInstance(KnownInstanceType::WrappedOptional(inner)) = value_type {
                value_type = inner.inner(db);
                none_chain_was_optional = true;
            }
            let none = Type::none(db, env);
            let narrowed = match value_type {
                Type::Union(u) => u.map(db, env, |elem| {
                    if elem.is_subtype_of(db, env, none) {
                        Type::Never
                    } else {
                        *elem
                    }
                }),
                ty if ty.is_subtype_of(db, env, none) => Type::Never,
                ty => ty,
            };
            if !narrowed.is_equivalent_to(db, env, value_type) {
                value_type = narrowed;
                none_chain_was_optional = true;
            }
        }

        // basedpython: `super.x` and `super[T].x` sugar in `.by` files.
        // overrides value_type with the corresponding bound-super type so that
        // attribute lookup is performed against the MRO, mirroring what the
        // transpile produces (`super().x` / `super(<predecessor>, self).x`)
        if self.is_basedpython_file()
            && let Some(super_value_type) = self.basedpython_super_value_type(value)
        {
            value_type = super_value_type;
        }

        // basedpython safe variance: a private member does not specialize. It keeps the type it
        // was declared with, and through any receiver but the class's own that type is erased to
        // what such a view actually knows — the parameter's bound
        if let Some(view) =
            crate::types::safe_variance::private_member_view(db, env, value_type, attr.as_str())
        {
            let read_type = if self.is_own_receiver_attribute(attribute) {
                view.declared_ty
            } else {
                view.read_type(db, env)
            };
            return Ok(self.basedpython_chain_result(
                attribute,
                read_type,
                none_chain_was_optional,
            ));
        }

        // basedpython: `expr.N` is tuple-member dot access. the parser only
        // produces ExprAttribute with a digit-only attr id in `.by` files, so
        // a successful lookup short-circuits the regular attribute resolution
        if self.is_basedpython_file()
            && !attr.id.as_str().is_empty()
            && attr.id.as_str().bytes().all(|b| b.is_ascii_digit())
            && let Ok(index) = attr.id.as_str().parse::<i32>()
            && let Some(spec) = value_type.exact_tuple_instance_spec(db)
            && let Ok(element_ty) = (&*spec).py_index(db, env, index)
        {
            return Ok(self.basedpython_chain_result(
                attribute,
                element_ty,
                none_chain_was_optional,
            ));
        }

        // basedpython: `T.a` in a type expression is an *attribute type* — the type of
        // member `a` on whatever `T` is specialized to. only a dotted name that *is* the
        // type expression qualifies, never one reached through nested value inference
        // (`Annotated`'s metadata). a parameter pack is excluded too: `P.args` /
        // `**Kwargs` name a pack's components rather than a member of it
        let mut attribute_type_receiver = None;
        if let Type::KnownInstance(KnownInstanceType::TypeVar(typevar)) = value_type {
            let is_attribute_type = self.is_basedpython_file()
                && self
                    .inference_flags()
                    .contains(InferenceFlags::RESOLVING_DOTTED_TYPE_EXPRESSION)
                && !typevar.is_parameter_pack(db)
                && !typevar.is_typevartuple(db);
            // binding the type parameter runs the lookup below — and any diagnostic for
            // a member the parameter cannot have — against its bound
            if (typevar.is_parameter_pack(db) || is_attribute_type)
                && let Some(bound_typevar) = bind_typevar(
                    db,
                    self.index,
                    self.scope().file_scope_id(db),
                    self.typevar_binding_context,
                    typevar,
                )
            {
                value_type = Type::TypeVar(bound_typevar);
                if is_attribute_type {
                    attribute_type_receiver = Some(value_type);
                }
            }
        }

        let mut assigned_type = None;
        if let Some(place_expr) = PlaceExpr::try_from_expr(attribute) {
            let (resolved, keys) =
                self.infer_place_load(place_expr, ast::ExprRef::Attribute(attribute));
            constraint_keys.extend(keys);
            if let Place::Defined(DefinedPlace {
                ty,
                definedness: Definedness::AlwaysDefined,
                ..
            }) = resolved.place
            {
                assigned_type = Some(ty);
            }
        }

        let mut fallback_place = value_type
            .try_member_lookup(db, env, &attr.id)
            .unwrap_or_else(|error| {
                error.report_diagnostic(&self.context, value_type, attribute, assigned_type);
                error.fallback_member(db)
            })
            .map_type(|ty| {
                self.narrow_expr_with_applicable_constraints(attribute, ty, &constraint_keys)
            });

        // basedpython: an attribute that resolves to no declared member may be
        // supplied by an `extension` in scope (this module's, or one from any
        // module imported with a plain `import mod`). extensions never shadow
        // declared members — this only runs after normal lookup came up empty
        if self.is_basedpython_file() && fallback_place.place.is_undefined() {
            if let Some(resolution) =
                extensions::resolve_extension_member(db, env, self.file(), value_type, &attr.id)
            {
                if let Some(other) = resolution.ambiguous_with
                    && let Some(builder) = self
                        .context
                        .report_lint(&AMBIGUOUS_EXTENSION_MEMBER, attribute)
                {
                    builder.into_diagnostic(format_args!(
                        "Attribute `{}` is supplied by more than one applicable \
                        extension of `{}`",
                        attr.id,
                        other.name(db),
                    ));
                }
                fallback_place = Place::bound(resolution.ty).into();
            }
        }

        // basedpython: `x.fn()` where `fn` is a name in scope declared as a
        // receiver callable accepting `x` — the access binds the receiver. like
        // extensions, this never shadows a declared member
        if self.is_basedpython_file()
            && fallback_place.place.is_undefined()
            && let Some(bound) = receivers::resolve_receiver_attribute(
                db,
                env,
                self.file(),
                self.scope(),
                value_type,
                &attr.id,
            )
        {
            fallback_place = Place::bound(bound).into();
        }

        let attr_name = &attr.id;
        let lookup_result = fallback_place.into_lookup_result(db, env);
        let resolved_type = lookup_result.unwrap_or_else(|lookup_err| {
            match lookup_err {
                LookupError::Undefined(_) => {
                    let fallback = || {
                        TypeAndQualifiers::new(
                            Type::unknown(),
                            TypeOrigin::Inferred,
                            TypeQualifiers::empty(),
                        )
                    };

                    let bound_on_instance = match value_type {
                        Type::ClassLiteral(class) => {
                            !class.instance_member(db, env, None, attr).is_undefined()
                        }
                        Type::SubclassOf(subclass_of @ SubclassOfType { .. }) => {
                            match subclass_of.subclass_of() {
                                SubclassOfInner::Class(class) => {
                                    !class.instance_member(db, env, attr).is_undefined()
                                }
                                SubclassOfInner::Dynamic(_) => unreachable!(
                                    "Attribute lookup on a dynamic `SubclassOf` type \
                                    should always return a bound symbol"
                                ),
                                SubclassOfInner::Protocol(_) => false,
                                SubclassOfInner::TypeVar(_) => false,
                            }
                        }
                        _ => false,
                    };

                    if let Type::ModuleLiteral(module) = value_type {
                        let module = module.module(db);
                        let module_name = module.name(db);
                        if module.kind(db).is_package()
                            && let Some(relative_submodule) = ModuleName::new(attr_name)
                        {
                            let mut maybe_submodule_name = module_name.clone();
                            maybe_submodule_name.extend(&relative_submodule);
                            if resolve_module(
                                db,
                                ImportingFile::File(
                                    self.file(),
                                    self.program_environment().resolver_environment(db),
                                ),
                                &maybe_submodule_name,
                            )
                            .is_some()
                            {
                                if let Some(builder) = self
                                    .context
                                    .report_lint(&POSSIBLY_MISSING_SUBMODULE, attribute)
                                {
                                    let mut diag = builder.into_diagnostic(format_args!(
                                        "Submodule `{attr_name}` might not have been imported"
                                    ));
                                    diag.help(format_args!(
                                        "Consider explicitly importing `{maybe_submodule_name}`"
                                    ));
                                }
                                return fallback();
                            }
                        }
                    }

                    if let Type::SpecialForm(special_form) = value_type {
                        if let Some(builder) =
                            self.context.report_lint(&UNRESOLVED_ATTRIBUTE, attribute)
                        {
                            let mut diag = builder.into_diagnostic(format_args!(
                                "Special form `{special_form}` has no attribute `{attr_name}`",
                            ));
                            if let Ok(defined_type) = value_type.in_type_expression(
                                db,
                                self.scope(),
                                self.typevar_binding_context,
                                self.inference_flags(),
                            ) && !defined_type.member(db, env, attr_name).place.is_undefined()
                            {
                                diag.help(format_args!(
                                    "Objects with type `{ty}` have a{maybe_n} `{attr_name}` \
                                    attribute, but the symbol `{special_form}` \
                                    does not itself inhabit the type `{ty}`",
                                    maybe_n = if attr_name.starts_with(['a', 'e', 'i', 'o', 'u']) {
                                        "n"
                                    } else {
                                        ""
                                    },
                                    ty = defined_type.display(db, env)
                                ));
                                if is_dotted_name(value) {
                                    let source =
                                        &source_text(self.db(), self.file())[value.range()];
                                    diag.help(format_args!(
                                        "This error may indicate that `{source}` was defined as \
                                        `{source} = {special_form}` when \
                                        `{source}: {special_form}` was intended"
                                    ));
                                }
                            }
                        }
                        return fallback();
                    }

                    let Some(builder) = self.context.report_lint(&UNRESOLVED_ATTRIBUTE, attribute)
                    else {
                        return fallback();
                    };

                    if bound_on_instance {
                        builder.into_diagnostic(format_args!(
                            "Attribute `{attr_name}` can only be accessed on instances, \
                            not on the class object `{}` itself.",
                            value_type.display(db, env)
                        ));
                        return fallback();
                    }

                    let mut diagnostic = match value_type {
                        Type::ModuleLiteral(module) => builder.into_diagnostic(format_args!(
                            "Module `{module_name}` has no member `{attr_name}`",
                            module_name = module.module(db).name(db),
                        )),
                        Type::ClassLiteral(class) => builder.into_diagnostic(format_args!(
                            "Class `{}` has no attribute `{attr_name}`",
                            class.name(db),
                        )),
                        Type::GenericAlias(alias) => builder.into_diagnostic(format_args!(
                            "Class `{}` has no attribute `{attr_name}`",
                            alias.display(db, env),
                        )),
                        Type::FunctionLiteral(function) => builder.into_diagnostic(format_args!(
                            "Function `{}` has no attribute `{attr_name}`",
                            function.name(db),
                        )),
                        _ => builder.into_diagnostic(format_args!(
                            "Object of type `{}` has no attribute `{attr_name}`",
                            value_type.display(db, env),
                        )),
                    };

                    if value_type.is_callable_type()
                        && KnownClass::FunctionType
                            .to_instance(db, env)
                            .member(db, env, attr_name)
                            .place
                            .is_definitely_bound()
                    {
                        diagnostic.help(format_args!(
                            "Function objects have a{maybe_n} `{attr_name}` attribute, \
                            but not all callable objects are functions",
                            maybe_n = if attr_name
                                .trim_start_matches('_')
                                .starts_with(['a', 'e', 'i', 'o', 'u'])
                            {
                                "n"
                            } else {
                                ""
                            },
                        ));

                        // without the <> around the URL, if you double click on the URL in the terminal it tries to load
                        // https://docs.astral.sh/ty/reference/typing-faq/#why-does-ty-say-callable-has-no-attribute-__name
                        // (without the __ suffix at the end of the URL). That doesn't exist, so the page loaded in the
                        // browser opens at the top of the FAQs page instead of taking you directly to the relevant FAQ.
                        diagnostic.help(
                            "See this FAQ for more information: \
                            <https://docs.astral.sh/ty/reference/typing-faq/\
                            #why-does-ty-say-callable-has-no-attribute-__name__>",
                        );
                    } else {
                        hint_if_stdlib_attribute_exists_on_other_versions(
                            db,
                            self.program_file(),
                            diagnostic,
                            value_type,
                            attr_name,
                            &format!("resolving the `{attr_name}` attribute"),
                        );
                    }

                    fallback()
                }
                LookupError::PossiblyUndefined(type_when_bound) => {
                    // `PossiblyUndefined` is ambiguous here. It could be because an attribute is
                    // conditionally defined, for example:
                    // ```
                    // class Foo:
                    //     if flag:
                    //         x = 42
                    // ```
                    // That is indeed a "possibly missing attribute", and it's a warning by default, because
                    // there's a high false positive rate.
                    //
                    // On the other hand, we could be looking at a union where some elements have
                    // the attribute but others definitely don't. That's a very different case, and
                    // we want it to be an error. Use `as_union_like` here to handle type aliases
                    // of unions and `NewType`s of float/complex in addition to explicit unions.
                    //
                    // Attribute lookup on a bounded type variable delegates to its upper bound, so
                    // use that bound here too when determining whether the lookup was on a union.
                    let union_like_type = if let Type::TypeVar(typevar) = value_type
                        && let Some(bound) = typevar.typevar(db).upper_bound(db, env)
                    {
                        bound
                    } else {
                        value_type
                    };

                    if let Some(union) = union_like_type.as_union_like(db) {
                        let mut elements_missing_the_attribute = FxIndexSet::default();
                        for element in union.elements(db) {
                            union_elements_missing_attribute(
                                db,
                                env,
                                *element,
                                attr_name,
                                &mut elements_missing_the_attribute,
                            );
                        }

                        if !elements_missing_the_attribute.is_empty() {
                            if let Some(builder) =
                                self.context.report_lint(&UNRESOLVED_ATTRIBUTE, attribute)
                            {
                                let missing_types = elements_missing_the_attribute
                                    .iter()
                                    .map(|ty| format!("`{}`", ty.display(db, env)))
                                    .collect::<Vec<_>>()
                                    .join(", ");

                                builder.into_diagnostic(format_args!(
                                    "Attribute `{attr_name}` is not defined on {} \
                                    in union `{union_like_type}`",
                                    missing_types,
                                    union_like_type = union_like_type.display(db, env),
                                ));
                            }
                            return type_when_bound;
                        }
                    }

                    report_possibly_missing_attribute(
                        &self.context,
                        attribute,
                        &attr.id,
                        value_type,
                    );

                    type_when_bound
                }
            }
        });

        let resolved_type = resolved_type.inner_type();

        self.check_deprecated(attr, resolved_type);

        // basedpython: an attribute type stays symbolic until the type parameter is
        // substituted, so `B[A2]().x` reads `a` off `A2` rather than off the bound the
        // lookup above resolved it against. it still goes through the chain result, so
        // a `?.` access composes here exactly as it does for every other receiver
        if let Some(receiver) = attribute_type_receiver {
            let attribute_type = DeferredType::build(
                db,
                env,
                &DeferredOperation::Attribute(attr.id.clone()),
                Box::from([receiver]),
            );
            return Ok(self.basedpython_chain_result(
                attribute,
                attribute_type,
                none_chain_was_optional,
            ));
        }

        // Even if we can obtain the attribute type based on the assignments, we still perform default type inference
        // (to report errors).
        let inferred_type = assigned_type.unwrap_or(resolved_type);

        // basedpython `?.`: short-circuit returns None on a None receiver, so
        // the overall expression type is the attribute type unioned with None
        let inferred_type =
            self.basedpython_chain_result(attribute, inferred_type, none_chain_was_optional);

        lookup_result
            .map(|_| inferred_type)
            .map_err(|_| inferred_type)
    }

    fn infer_attribute_expression(&mut self, attribute: &ast::ExprAttribute) -> Type<'db> {
        let ast::ExprAttribute {
            value,
            attr,
            range: _,
            node_index: _,
            ctx,
            optional: _,
        } = attribute;

        match ctx {
            ExprContext::Load => self
                .infer_attribute_load(attribute)
                .unwrap_or_else(|recovery_ty| recovery_ty),
            ExprContext::Store => {
                self.infer_expression(value, TypeContext::default());
                Type::Never
            }
            ExprContext::Del => {
                let _ = self.infer_attribute_load(attribute);
                self.validate_attribute_deletion(
                    attribute,
                    self.expression_type(value),
                    attr.as_str(),
                    true,
                );
                Type::Never
            }
            ExprContext::Invalid => {
                self.infer_expression(value, TypeContext::default());
                Type::unknown()
            }
        }
    }

    fn report_unsupported_unary_operator(
        &self,
        unary: &ast::ExprUnaryOp,
        op: ast::UnaryOp,
        operand_type: Type<'db>,
        unary_dunder_method: &str,
        error: Option<&CallDunderError<'db>>,
    ) {
        let db = self.db();
        let env = self.program_environment();
        let Some(builder) = self.context.report_lint(&UNSUPPORTED_OPERATOR, unary) else {
            return;
        };

        let mut diagnostic = builder.into_diagnostic(format_args!(
            "Unary operator `{op}` is not supported for object of type `{}`",
            operand_type.display(db, env),
        ));

        if let Some(CallDunderError::PossiblyUnbound {
            unbound_on: Some(unbound_on),
            ..
        }) = error
        {
            for ty in unbound_on.iter().copied() {
                diagnostic.info(format_args!(
                    "`{}` does not implement `{unary_dunder_method}`",
                    ty.display(db, env)
                ));
            }
        }
    }

    /// basedpython: the type a unary operator evaluates to when an applicable
    /// extension supplies its dunder. `None` outside a basedpython file, when
    /// no extension supplies it, or when the resolved member does not accept
    /// the call — each of which leaves the operator unsupported, exactly as it
    /// is without the extension
    pub(super) fn try_unary_extension_operator(
        &self,
        op: ast::UnaryOp,
        operand: Type<'db>,
    ) -> Option<Type<'db>> {
        let env = self.program_environment();
        if !self.is_basedpython_file() {
            return None;
        }
        let db = self.db();
        extensions::unary_extension_operator(db, env, self.file(), op, operand)?.return_type(
            db,
            env,
            &CallArguments::none(),
        )
    }

    /// basedpython: the type a binary operator evaluates to when an applicable
    /// extension supplies its dunder, on either operand
    pub(super) fn try_binary_extension_operator(
        &self,
        left: Type<'db>,
        op: ast::Operator,
        right: Type<'db>,
    ) -> Option<Type<'db>> {
        let env = self.program_environment();
        if !self.is_basedpython_file() {
            return None;
        }
        let db = self.db();
        let operator =
            extensions::binary_extension_operator(db, env, self.file(), left, op, right)?;
        let argument = if operator.reflected { left } else { right };
        operator.return_type(db, env, &CallArguments::positional([argument]))
    }

    /// basedpython: the type a comparison evaluates to when an applicable
    /// extension supplies its dunder. A membership test coerces
    /// `__contains__`'s result, so it is a `bool` whatever the extension
    /// declares
    pub(super) fn try_comparison_extension_operator(
        &self,
        left: Type<'db>,
        op: ast::CmpOp,
        right: Type<'db>,
    ) -> Option<Type<'db>> {
        let env = self.program_environment();
        if !self.is_basedpython_file() {
            return None;
        }
        let db = self.db();
        let operator =
            extensions::comparison_extension_operator(db, env, self.file(), left, op, right)?;
        let argument = if operator.reflected { left } else { right };
        let returned = operator.return_type(db, env, &CallArguments::positional([argument]))?;
        Some(match op {
            ast::CmpOp::In | ast::CmpOp::NotIn => KnownClass::Bool.to_instance(db, env),
            _ => returned,
        })
    }

    fn infer_unary_expression(&mut self, unary: &ast::ExprUnaryOp) -> Type<'db> {
        let ast::ExprUnaryOp {
            range: _,
            node_index: _,
            op,
            operand,
        } = unary;

        let operand_type = self.infer_expression(operand, TypeContext::default());

        self.infer_unary_expression_type(*op, operand_type, unary)
    }

    fn infer_unary_expression_type(
        &mut self,
        op: ast::UnaryOp,
        operand_type: Type<'db>,
        unary: &ast::ExprUnaryOp,
    ) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let fallback_unary_expression_type = || {
            let unary_dunder_method = match op {
                ast::UnaryOp::Invert => "__invert__",
                ast::UnaryOp::UAdd => "__pos__",
                ast::UnaryOp::USub => "__neg__",
                ast::UnaryOp::Not => {
                    unreachable!("Not operator is handled in its own case");
                }
                ast::UnaryOp::Optional | ast::UnaryOp::Propagate | ast::UnaryOp::Force => {
                    unreachable!("basedpython postfix operators are handled in their own case");
                }
            };

            match operand_type.try_call_dunder(
                db,
                env,
                unary_dunder_method,
                CallArguments::none(),
                TypeContext::default(),
            ) {
                Ok(outcome) => outcome.return_type(db, env),
                Err(e) => {
                    // basedpython: an applicable extension may supply the dunder
                    if let Some(ty) = self.try_unary_extension_operator(op, operand_type) {
                        return ty;
                    }
                    self.report_unsupported_unary_operator(
                        unary,
                        op,
                        operand_type,
                        unary_dunder_method,
                        Some(&e),
                    );
                    e.fallback_return_type(db, env)
                }
            }
        };

        match (op, operand_type) {
            // parameter-only marker; behaves as the type a body sees (bound of `Key`)
            (_, Type::Overlapping(overlapping)) => overlapping.value_type(self.db(), env),
            (_, Type::Restricted(restricted)) => {
                self.infer_unary_expression_type(op, restricted.value_type(self.db()), unary)
            }
            // basedpython: the counterpart of the symbolic arithmetic in
            // `infer_binary_expression_type_impl` — `-i` has to build the same operation the
            // annotation `-> -I` builds, or the two could never be compared
            (op, operand_type)
                if self.is_basedpython_file()
                    && DeferredOperation::Unary(op).is_checked_arithmetic()
                    && is_symbolic_operand(operand_type)
                    && is_integer_operand(self.db(), env, operand_type) =>
            {
                DeferredType::build(
                    self.db(),
                    env,
                    &DeferredOperation::Unary(op),
                    Box::new([operand_type]),
                )
            }
            (_, Type::Deferred(deferred)) => deferred.reduced(self.db(), env),
            (ast::UnaryOp::Invert | ast::UnaryOp::UAdd | ast::UnaryOp::USub, Type::Dynamic(_))
            | (_, Type::Divergent(_)) => operand_type,
            (_, Type::Never) => Type::Never,

            (_, Type::TypeAlias(alias)) => {
                self.infer_unary_expression_type(op, alias.value_type(db), unary)
            }

            // basedpython postfix `!` force-unwrap and `^` propagate both peel
            // one absent layer. On a `WrappedOptional` they yield the wrapped
            // inner type; on a union they strip the absent arms — `None` for an
            // optional (`T | None`) and any `BaseException` subtype for a
            // result-like union (`int | TypeError`) — leaving the present value
            (
                ast::UnaryOp::Force | ast::UnaryOp::Propagate,
                Type::KnownInstance(KnownInstanceType::WrappedOptional(inner)),
            ) => inner.inner(self.db()),
            (ast::UnaryOp::Force | ast::UnaryOp::Propagate, Type::Union(union)) => {
                let none = Type::none(self.db(), env);
                let base_exception = KnownClass::BaseException.to_instance(self.db(), env);
                let is_absent = |element: Type<'db>| {
                    element == none
                        || (!element.is_dynamic()
                            && element.is_subtype_of(self.db(), env, base_exception))
                };
                let present: Vec<Type<'db>> = union
                    .elements(self.db())
                    .iter()
                    .copied()
                    .filter(|element| !is_absent(*element))
                    .collect();
                if present.len() == union.elements(self.db()).len() {
                    // nothing to unwrap — unwrap of a non-optional union
                    todo_type!("basedpython unwrap of non-optional")
                } else {
                    UnionType::from_elements(self.db(), env, present)
                }
            }

            // basedpython postfix `?` and `^` / `!` on non-optional operands.
            // These are wrapped-type surface syntax that the transpiler lowers
            // away; ty does not yet model the remaining unwrap/propagate cases
            (ast::UnaryOp::Optional | ast::UnaryOp::Propagate | ast::UnaryOp::Force, _) => {
                todo_type!("basedpython wrapped-type operator")
            }

            (
                ast::UnaryOp::UAdd | ast::UnaryOp::USub | ast::UnaryOp::Invert,
                Type::LiteralValue(literal),
            ) => binary_expressions::literal_unary_op(self.db(), env, op, literal)
                .unwrap_or_else(fallback_unary_expression_type),

            (ast::UnaryOp::Invert, Type::KnownInstance(KnownInstanceType::ConstraintSet(set))) => {
                let constraints = ConstraintSetBuilder::new();
                let result = constraints.into_owned(|constraints| {
                    let set = constraints.load(db, env, set.constraints(self.db()));
                    set.negate(self.db(), constraints)
                });
                Type::KnownInstance(KnownInstanceType::ConstraintSet(
                    InternedConstraintSet::new(self.db(), result),
                ))
            }

            (ast::UnaryOp::Not, ty) => Type::from_truthiness(
                db,
                env,
                ty.try_bool(db, env)
                    .unwrap_or_else(|err| {
                        err.report_diagnostic(&self.context, unary);
                        err.fallback_truthiness()
                    })
                    .negate(),
            ),
            // Handle constrained TypeVars specially: check each constraint individually.
            //
            // TODO: We expect to replace this with more general support once we migrate to the new
            // solver.
            (
                op @ (ast::UnaryOp::UAdd | ast::UnaryOp::USub | ast::UnaryOp::Invert),
                Type::TypeVar(tvar),
            ) => {
                let unary_dunder_method = match op {
                    ast::UnaryOp::Invert => "__invert__",
                    ast::UnaryOp::UAdd => "__pos__",
                    ast::UnaryOp::USub => "__neg__",
                    ast::UnaryOp::Not => unreachable!(),
                    ast::UnaryOp::Optional | ast::UnaryOp::Propagate | ast::UnaryOp::Force => {
                        unreachable!()
                    }
                };

                match tvar.typevar(self.db()).bound_or_constraints(db, env) {
                    Some(TypeVarBoundOrConstraints::Constraints(constraints)) => {
                        match Self::map_constrained_typevar_constraints(
                            db,
                            env,
                            operand_type,
                            constraints,
                            |constraint| {
                                constraint
                                    .try_call_dunder(
                                        db,
                                        env,
                                        unary_dunder_method,
                                        CallArguments::none(),
                                        TypeContext::default(),
                                    )
                                    .map(|outcome| outcome.return_type(db, env))
                                    .ok()
                            },
                        ) {
                            Some(ty) => ty,
                            None => {
                                // At least one constraint failed; report error.
                                self.report_unsupported_unary_operator(
                                    unary,
                                    op,
                                    operand_type,
                                    unary_dunder_method,
                                    None,
                                );
                                operand_type
                                    .try_call_dunder(
                                        db,
                                        env,
                                        unary_dunder_method,
                                        CallArguments::none(),
                                        TypeContext::default(),
                                    )
                                    .map_or_else(
                                        |e| e.fallback_return_type(db, env),
                                        |b| b.return_type(db, env),
                                    )
                            }
                        }
                    }
                    // For bounded TypeVars with union bounds (like `bound=float` which becomes
                    // `int | float`), we need to delegate to the bound type.
                    Some(TypeVarBoundOrConstraints::UpperBound(bound)) => {
                        self.infer_unary_expression_type(op, bound, unary)
                    }
                    // For unconstrained TypeVars, fall through to default handling.
                    None => {
                        match operand_type.try_call_dunder(
                            db,
                            env,
                            unary_dunder_method,
                            CallArguments::none(),
                            TypeContext::default(),
                        ) {
                            Ok(outcome) => outcome.return_type(db, env),
                            Err(e) => {
                                self.report_unsupported_unary_operator(
                                    unary,
                                    op,
                                    operand_type,
                                    unary_dunder_method,
                                    Some(&e),
                                );
                                e.fallback_return_type(db, env)
                            }
                        }
                    }
                }
            }

            (
                ast::UnaryOp::UAdd | ast::UnaryOp::USub | ast::UnaryOp::Invert,
                Type::FunctionLiteral(_)
                | Type::Callable(..)
                | Type::WrapperDescriptor(_)
                | Type::KnownBoundMethod(_)
                | Type::DataclassDecorator(_)
                | Type::DataclassTransformer(_)
                | Type::BoundMethod(_)
                | Type::ModuleLiteral(_)
                | Type::ClassLiteral(_)
                | Type::GenericAlias(_)
                | Type::SubclassOf(_)
                | Type::NominalInstance(_)
                | Type::ProtocolInstance(_)
                | Type::SpecialForm(_)
                | Type::KnownInstance(_)
                | Type::PropertyInstance(_)
                | Type::Union(_)
                | Type::Intersection(_)
                // the dunder lookup resolves across the materializations
                | Type::UnsafeUnion(_)
                | Type::EnumComplement(_)
                | Type::AlwaysTruthy
                | Type::AlwaysFalsy
                | Type::BoundSuper(_)
                | Type::TypeIs(_)
                | Type::TypeGuard(_)
                | Type::TypeForm(_)
                | Type::TypedDict(_)
                | Type::NewTypeInstance(_),
            ) => fallback_unary_expression_type(),
        }
    }

    fn infer_boolean_expression(
        &mut self,
        bool_op: &ast::ExprBoolOp,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let ast::ExprBoolOp {
            range: _,
            node_index: _,
            op,
            values,
        } = bool_op;
        // The first operand has no peers. If no later operand is a collection literal,
        // accumulating prior types cannot affect inference.
        let track_peer_types = is_empty_collection_type_context(tcx)
            && values.iter().skip(1).any(is_collection_literal);
        self.infer_chained_boolean_types(
            *op,
            track_peer_types,
            values.iter().enumerate(),
            |(_, value)| is_collection_literal(value),
            |builder, (index, value), peer_ty| {
                let ty = if index == values.len() - 1 {
                    builder
                        .infer_expression_with_collection_literal_peer_context(value, tcx, peer_ty)
                } else {
                    builder.infer_maybe_standalone_expression_with_collection_literal_peer_context(
                        value, tcx, peer_ty,
                    )
                };

                (ty, value.range())
            },
        )
    }

    /// Computes the output of a chain of (one) boolean operation, consuming as input an iterator
    /// of operations and calling the `infer_ty` for each to infer their types.
    /// The iterator is consumed even if the boolean evaluation can be short-circuited,
    /// in order to ensure the invariant that all expressions are evaluated when inferring types.
    ///
    /// `infer_ty` receives the unguarded union of previous operand types that may contribute to the
    /// result. This can be used as a type context without losing generic specialization information
    /// to the truthiness guards applied to the final result type. `needs_peer_type` determines
    /// whether that union is materialized for each operand.
    fn infer_chained_boolean_types<Iterator, Item, NeedsPeerType, InferType>(
        &mut self,
        op: ast::BoolOp,
        track_peer_types: bool,
        operations: Iterator,
        needs_peer_type: NeedsPeerType,
        infer_ty: InferType,
    ) -> Type<'db>
    where
        Iterator: IntoIterator<Item = Item>,
        NeedsPeerType: Fn(&Item) -> bool,
        InferType: Fn(&mut Self, Item, Option<Type<'db>>) -> (Type<'db>, TextRange),
    {
        let db = self.db();
        let env = self.program_environment();
        let mut done = false;
        let mut peer_types: Option<UnionAccumulator<'db>> = None;

        let elements = operations
            .into_iter()
            .with_position()
            .map(|(position, item)| {
                let peer_ty = if done || !track_peer_types || !needs_peer_type(&item) {
                    None
                } else {
                    peer_types
                        .as_mut()
                        .map(|peer_types| peer_types.get_or_build(db, env))
                };
                let (ty, range) = infer_ty(self, item, peer_ty);

                let is_last = position.is_last();

                if is_last {
                    if done { Type::Never } else { ty }
                } else {
                    let truthiness = ty.try_bool(db, env).unwrap_or_else(|err| {
                        err.report_diagnostic(&self.context, range);
                        err.fallback_truthiness()
                    });

                    if done {
                        return Type::Never;
                    }

                    match (truthiness, op) {
                        (Truthiness::AlwaysTrue, ast::BoolOp::And) => Type::Never,
                        (Truthiness::AlwaysFalse, ast::BoolOp::Or) => Type::Never,

                        (Truthiness::AlwaysFalse, ast::BoolOp::And)
                        | (Truthiness::AlwaysTrue, ast::BoolOp::Or) => {
                            done = true;
                            ty
                        }

                        (Truthiness::Ambiguous, _) => {
                            if track_peer_types {
                                match &mut peer_types {
                                    Some(peer_types) => peer_types.add(db, env, ty),
                                    None => peer_types = Some(UnionAccumulator::new(ty)),
                                }
                            }
                            IntersectionBuilder::new(db, env)
                                .add_positive(ty)
                                .add_negative(match op {
                                    ast::BoolOp::And => Type::AlwaysTruthy,
                                    ast::BoolOp::Or => Type::AlwaysFalsy,
                                })
                                .build()
                        }
                    }
                }
            });

        UnionType::from_elements(db, env, elements)
    }

    fn infer_compare_expression(&mut self, compare: &ast::ExprCompare) -> Type<'db> {
        let db = self.db();
        let ast::ExprCompare {
            range: _,
            node_index: _,
            left,
            ops,
            comparators,
        } = compare;

        self.infer_expression(left, TypeContext::default());

        // https://docs.python.org/3/reference/expressions.html#comparisons
        // > Formally, if `a, b, c, …, y, z` are expressions and `op1, op2, …, opN` are comparison
        // > operators, then `a op1 b op2 c ... y opN z` is equivalent to `a op1 b and b op2 c and
        // ... > y opN z`, except that each expression is evaluated at most once.
        //
        // As some operators (==, !=, <, <=, >, >=) *can* return an arbitrary type, the logic below
        // is shared with the one in `infer_binary_type_comparison`.
        //
        // A chain like `a == True == b` is two comparisons over one literal: reporting each pair
        // would double up on that `True`, and "test the operand" is not the fix for the chain.
        let single_comparison = ops.len() == 1;
        self.infer_chained_boolean_types(
            ast::BoolOp::And,
            false,
            std::iter::once(&**left)
                .chain(comparators)
                .tuple_windows::<(_, _)>()
                .zip(ops),
            |_| false,
            |builder, ((left, right), op), _peer_ty| {
                let left_ty = builder.expression_type(left);
                let right_ty = builder.infer_expression(right, TypeContext::default());

                let range = TextRange::new(left.start(), right.end());

                if single_comparison {
                    builder.check_redundant_boolean_comparison(
                        left, right, left_ty, right_ty, *op, range,
                    );
                }

                // a basedpython keyword-form `is`/`is not` whose rhs is a
                // class (or a parametric test like `x is list[int]`) is an
                // instance check, not python identity: it always yields a
                // `bool`, and its reachability is decided by narrowing (not
                // by the instance-vs-class-object disjointness that would
                // otherwise type it `Literal[False]` and kill a live branch)
                if let Some(ty) =
                    builder.check_basedpython_is_test(left, right, left_ty, right_ty, *op)
                {
                    return (ty, range);
                }

                let ty = comparisons::infer_binary_type_comparison(
                    &builder.context,
                    left_ty,
                    *op,
                    right_ty,
                    range,
                )
                // basedpython: an applicable extension may supply the dunder.
                // only for a lone comparison — a chain is two calls joined by a
                // short-circuit, which the lowering does not build, so accepting
                // one would put the checker and the runtime at odds
                .or_else(|error| {
                    if single_comparison {
                        builder
                            .try_comparison_extension_operator(left_ty, *op, right_ty)
                            .ok_or(error)
                    } else {
                        Err(error)
                    }
                })
                .unwrap_or_else(|error| {
                    report_unsupported_comparison(
                        &builder.context,
                        &error,
                        range,
                        left,
                        right,
                        left_ty,
                        right_ty,
                    );

                    match op {
                        // `in, not in, is, is not` always return bool instances
                        ast::CmpOp::In | ast::CmpOp::NotIn | ast::CmpOp::Is | ast::CmpOp::IsNot => {
                            KnownClass::Bool.to_instance(db, builder.program_environment())
                        }
                        // Other operators can return arbitrary types
                        _ => Type::unknown(),
                    }
                });

                (ty, range)
            },
        )
    }

    /// basedpython: type a keyword-form `is`/`is not` pair that performs an
    /// *instance check* rather than python identity, and check it. Returns
    /// `Some(bool)` — the runtime result type — for any such pair, so the
    /// identity folds (disjointness → `Literal[False]`) never apply to it;
    /// reachability is decided by narrowing instead
    fn check_basedpython_is_test(
        &mut self,
        left: &ast::Expr,
        right: &ast::Expr,
        left_ty: Type<'db>,
        right_ty: Type<'db>,
        op: ast::CmpOp,
    ) -> Option<Type<'db>> {
        let (bool_ty, decision) =
            self.classify_basedpython_is_test(left, right, left_ty, right_ty, op)?;
        self.report_non_overlapping_type_test(left, right, left_ty, right_ty, op, decision);
        Some(bool_ty)
    }

    /// The type a keyword-form `is`/`is not` asks its left operand to have: the
    /// instance type of the class on the right, or the union of the arms' instance
    /// types for a union target. `None` for a target with no instance form.
    fn is_test_target_instance(&self, right: &ast::Expr, right_ty: Type<'db>) -> Option<Type<'db>> {
        let env = self.program_environment();
        // an over-approximating projection is the safe direction here: a wider
        // target can only overlap more, so it never invents a disjointness
        let db = self.db();
        let Some(arms) = union_target_arms(right) else {
            return right_ty
                .to_instance(db, env)
                .map(InstanceProjection::into_inner);
        };
        let mut instances = Vec::with_capacity(arms.len());
        for arm in arms {
            instances.push(self.expression_type(arm).to_instance(db, env)?.into_inner());
        }
        Some(UnionType::from_elements(db, env, instances))
    }

    /// Warn when a keyword-form `is`/`is not` tests a value against a type it can
    /// never have. The test is then a constant — `is` never holds and `is not`
    /// always does — so either the guarded branch is dead or the wrong type was
    /// named. `Any`/`Unknown` overlap everything, so those never fire.
    fn report_non_overlapping_type_test(
        &self,
        left: &ast::Expr,
        right: &ast::Expr,
        left_ty: Type<'db>,
        right_ty: Type<'db>,
        op: ast::CmpOp,
        decision: IsTestDecision,
    ) {
        let env = self.program_environment();
        let db = self.db();
        let Some(target) = self.is_test_target_instance(right, right_ty) else {
            return;
        };
        let never_holds = match decision {
            IsTestDecision::Instance => left_ty.is_disjoint_from(db, env, target),
            IsTestDecision::ParametricNeverHolds => true,
            IsTestDecision::Undecided => false,
        };
        if !never_holds {
            return;
        }
        let range = TextRange::new(left.start(), right.end());
        let Some(builder) = self.context.report_lint(&NON_OVERLAPPING_TYPE_TEST, range) else {
            return;
        };
        let always = if op == ast::CmpOp::Is {
            "False"
        } else {
            "True"
        };
        builder.into_diagnostic(format_args!(
            "`{}` and `{}` are non-overlapping types, so this test is always `{always}`",
            left_ty.display(db, env),
            target.display(db, env),
        ));
    }

    /// Decide whether this pair is an instance check, erroring when the pair is a
    /// parametric test (`x is list[int]`) against a builtin collection whose
    /// runtime instances erase their type arguments, so no runtime probe of the
    /// value can ever confirm the specialization. `None` when the pair keeps
    /// python identity semantics (`===` spelling, a literal or other plain-value
    /// rhs such as an enum member — mirroring the transpiler's lowering), so the
    /// caller keeps its usual comparison typing
    fn classify_basedpython_is_test(
        &mut self,
        left: &ast::Expr,
        right: &ast::Expr,
        left_ty: Type<'db>,
        right_ty: Type<'db>,
        op: ast::CmpOp,
    ) -> Option<(Type<'db>, IsTestDecision)> {
        let env = self.program_environment();
        if !matches!(op, ast::CmpOp::Is | ast::CmpOp::IsNot) || !self.is_basedpython_file() {
            return None;
        }
        // a literal rhs (`x is None`, `x is 0`) keeps python identity
        // semantics; the transpiler leaves the operator untouched
        if right.is_literal_expr() {
            return None;
        }
        let source = ruff_db::source::source_text(self.db(), self.file());
        if !crate::reified::is_keyword_comparison(source.as_str(), op, left, right) {
            return None;
        }
        let bool_ty = KnownClass::Bool.to_instance(self.db(), env);

        // a union target `a is T1 | T2` tests each arm (`type(a) <: Ti` for any
        // arm). an erased arm can't be checked at runtime and, unlike a
        // standalone erased target, may not fold to a constant inside the
        // disjunction — that would be unsound — so it is rejected per arm
        if let Some(arms) = union_target_arms(right) {
            let mut decision = IsTestDecision::Instance;
            for arm in arms {
                let Some(alias) = crate::types::reified_infer::parametric_is_target(
                    self.db(),
                    env,
                    self.expression_type(arm),
                ) else {
                    continue;
                };
                // a parametric arm is decided by the engine rather than by
                // disjointness, and the test holds as soon as *any* arm does, so
                // one such arm puts the whole disjunction out of the lint's reach
                decision = IsTestDecision::Undecided;
                if let crate::types::reified_infer::ParametricIsPlan::ErasedTarget(reason) =
                    crate::types::reified_infer::classify_parametric_is(
                        self.db(),
                        env,
                        self.file(),
                        left_ty,
                        alias,
                        arm,
                    )
                {
                    self.report_erased_type_check(arm.range(), &source[arm.range()], reason);
                }
            }
            return Some((bool_ty, decision));
        }

        // a plain-value rhs (an enum member, an instance of a non-type class)
        // keeps python identity semantics — the transpiler leaves `is`/`is not`
        // untouched, so ty types it as an ordinary identity comparison too
        if crate::types::basedpython_is_keeps_identity(self.db(), env, right_ty) {
            return None;
        }

        let Some(alias) =
            crate::types::reified_infer::parametric_is_target(self.db(), env, right_ty)
        else {
            // a bare class / dynamic rhs (`x is int`, `x is SomeClass`) is an
            // instance check that lowers to `isinstance`, so it always yields a
            // `bool` — the identity folds (disjointness → `Literal[False]`)
            // must not apply
            return Some((bool_ty, IsTestDecision::Instance));
        };
        let plan = crate::types::reified_infer::classify_parametric_is(
            self.db(),
            env,
            self.file(),
            left_ty,
            alias,
            right,
        );
        // only a probe against a runtime-erased target is an error; every
        // other plan (fold, reified-cell equality, witness, or a probe of a
        // user generic that carries `__orig_class__`) is a valid test
        if let crate::types::reified_infer::ParametricIsPlan::ErasedTarget(reason) = plan {
            self.report_erased_type_check(
                TextRange::new(left.start(), right.end()),
                &source[right.range()],
                reason,
            );
        }
        let decision = if plan == crate::types::reified_infer::ParametricIsPlan::Fold(false) {
            IsTestDecision::ParametricNeverHolds
        } else {
            IsTestDecision::Undecided
        };
        Some((bool_ty, decision))
    }

    /// report an `erased-type-check` for a parametric `is`-target (or one arm
    /// of a union target) that has no runtime residue — either because the
    /// target records no specialization to probe, or because the target cannot
    /// be spelled at runtime at all. every other concrete class records its
    /// specialization on the instance or across its mro, so the runtime probe
    /// unwinds it instead
    fn report_erased_type_check(
        &self,
        primary: TextRange,
        target: &str,
        reason: ErasedTargetReason,
    ) {
        let Some(builder) = self.context.report_lint(&ERASED_TYPE_CHECK, primary) else {
            return;
        };
        match reason {
            ErasedTargetReason::Protocol => {
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "`is {target}` cannot be checked at runtime: a protocol's instances don't \
                     record which specialization they satisfy"
                ));
                diagnostic.info(format_args!(
                    "an instance's `__orig_class__` names its concrete class, never the protocol, \
                     and a structural `isinstance` check can't see type arguments"
                ));
                diagnostic.info(format_args!(
                    "reify the type parameter (`def f[T](x: T)`), or test against a concrete class \
                     that records the specialization on its instances or across its mro"
                ));
            }
            ErasedTargetReason::NotSubscriptable => {
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "`is {target}` cannot be checked at runtime: the target class cannot be \
                     subscripted"
                ));
                diagnostic.info(format_args!(
                    "the class has no `__class_getitem__` on this python version, so the test's \
                     runtime check would raise `TypeError` evaluating `{target}`"
                ));
                diagnostic.info(format_args!(
                    "drop the type arguments and test against the bare class, which is all the \
                     runtime records anyway"
                ));
            }
            ErasedTargetReason::BuiltinCollection => {
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "`is {target}` cannot be checked at runtime: a builtin collection's instances \
                     don't record their type arguments"
                ));
                diagnostic.info(format_args!(
                    "test against a subclass that fixes the arguments (`class B(list[int])`), \
                     whose `__orig_bases__` the probe can unwind"
                ));
            }
        }
    }

    /// `reification` names the owner of this list, which decides whether a
    /// basedpython `reified` parameter — or a declared variance — can take
    /// effect; one that cannot is reported here rather than silently dropped.
    fn infer_type_parameters(
        &mut self,
        type_parameters: &ast::TypeParams,
        reification: TypeParamReification,
    ) {
        let ast::TypeParams {
            range: _,
            node_index: _,
            type_params,
            separators: _,
        } = type_parameters;
        // the modifier is basedpython-only surface syntax, so a `.py` file that spells it
        // already has the parser's error and this would only pile on
        let source_type = self.file().source_type(self.db());
        for type_param in type_params {
            if source_type.is_basedpython()
                && type_param.is_reified()
                && let Some(reason) = reification.rejection(type_param, source_type)
                && let Some(builder) = self
                    .context
                    .report_lint(&INVALID_REIFIED_TYPE_PARAM, type_param)
            {
                let name = type_param.name();
                let mut diagnostic = builder
                    .into_diagnostic(format_args!("Type parameter `{name}` cannot be reified"));
                diagnostic.info(reason);
            }
            if source_type.is_basedpython()
                && let ast::TypeParam::TypeVar(type_var) = type_param
                && type_var.variance.is_some()
                && let Some(reason) = reification.variance_rejection()
                && let Some(builder) = self
                    .context
                    .report_lint(&INVALID_VARIANCE_DECLARATION, type_param)
            {
                let name = type_param.name();
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "Type parameter `{name}` cannot declare a variance"
                ));
                diagnostic.info(reason);
            }
            match type_param {
                ast::TypeParam::TypeVar(node) => self.infer_definition(node),
                ast::TypeParam::ParamSpec(node) => self.infer_definition(node),
                ast::TypeParam::TypeVarTuple(node) => self.infer_definition(node),
            }
        }
    }

    pub(super) fn finish_expression(mut self) -> ExpressionInference<'db> {
        self.infer_region();
        self.into_expression_inference()
    }

    /// Consume the results already collected by this builder without inferring its region.
    fn into_expression_inference(self) -> ExpressionInference<'db> {
        let region = self.region;
        self.into_expression_cache_entry()
            .into_expression_inference(region)
    }

    /// Consume the results already collected by this builder without compacting them.
    fn into_expression_cache_entry(self) -> FullExpressionCacheEntry<'db> {
        let Self {
            context,
            expressions,
            qualifiers: _,
            type_expression_flags,
            collection_use_constraints,
            fluid_adoptions,
            fluid_creation,
            fluid_timeline,
            string_annotations,
            unsolved_typevar_calls,
            expected_types,
            trailing_lambda_return: _,
            scope,
            bindings,
            declarations,
            deferred,
            cycle_recovery,
            dataclass_field_specifiers: _,
            slice_materialization: _,

            // Ignored; only relevant to definition regions
            undecorated_type: _,
            discards_dict_key_assignments: _,

            // builder only state
            expression_cache: _,
            basedpython_chain_present: _,
            reachability_cache: _,
            typevar_binding_context: _,
            deferred_state: _,
            called_functions,
            index: _,
            region: _,
            return_types_and_ranges: _,
        } = self;

        let diagnostics = context.finish_uncompacted();
        let _ = scope;

        assert!(
            declarations.is_empty(),
            "Expression region can't have declarations"
        );
        assert!(
            deferred.is_empty(),
            "Expression region can't have deferred definitions"
        );

        FullExpressionCacheEntry {
            expressions,
            type_expression_flags,
            collection_use_constraints,
            fluid_adoptions,
            fluid_creation,
            fluid_timeline,
            string_annotations,
            unsolved_typevar_calls,
            expected_types,
            bindings,
            diagnostics,
            called_functions,
            cycle_recovery,
            #[cfg(debug_assertions)]
            scope,
        }
    }

    pub(super) fn finish_statement(mut self) -> StatementInferenceInner<'db> {
        self.infer_region();

        let Self {
            context,
            expressions,
            qualifiers,
            type_expression_flags,
            fluid_creation: _,
            fluid_timeline: _,
            mut fluid_adoptions,
            mut collection_use_constraints,
            string_annotations,
            expected_types,
            trailing_lambda_return: _,
            scope,
            bindings,
            declarations,
            deferred,
            cycle_recovery,
            called_functions,
            mut return_types_and_ranges,
            unsolved_typevar_calls: _,

            // Ignored; only relevant to definition regions
            undecorated_type: _,
            discards_dict_key_assignments: _,

            // builder only state
            expression_cache: _,
            basedpython_chain_present: _,
            reachability_cache: _,
            dataclass_field_specifiers: _,
            slice_materialization: _,
            typevar_binding_context: _,
            deferred_state: _,
            index: _,
            region: _,
        } = self;

        let _ = scope;
        let diagnostics = context.finish();

        let extra = (!diagnostics.is_empty()
            || !string_annotations.is_empty()
            || cycle_recovery.is_some()
            || !expected_types.is_empty()
            || !deferred.is_empty()
            || !called_functions.is_empty()
            || !return_types_and_ranges.is_empty()
            || !qualifiers.is_empty()
            || !type_expression_flags.is_empty()
            || !collection_use_constraints.is_empty()
            || !fluid_adoptions.is_empty())
        .then(|| {
            collection_use_constraints.shrink_to_fit();
            fluid_adoptions.shrink_to_fit();
            return_types_and_ranges.shrink_to_fit();
            Box::new(StatementInferenceInnerExtra {
                string_annotations: FrozenSet::from(string_annotations),
                fluid_adoptions,
                expected_types: FrozenMap::from(expected_types),
                called_functions: called_functions
                    .into_iter()
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_types_and_ranges: return_types_and_ranges.into_boxed_slice(),
                type_expression_flags: FrozenMap::from(type_expression_flags),
                collection_use_constraints,
                cycle_recovery,
                deferred: deferred.into_boxed_slice(),
                diagnostics,
                qualifiers: FrozenMap::from(qualifiers),
            })
        });

        if bindings.len() > 20 {
            tracing::debug!(
                "Inferred statement region `{:?}` contains {} bindings. \
                Lookups by linear scan might be slow.",
                self.region,
                bindings.len(),
            );
        }

        if declarations.len() > 20 {
            tracing::debug!(
                "Inferred statement region `{:?}` contains {} declarations. \
                Lookups by linear scan might be slow.",
                self.region,
                declarations.len(),
            );
        }

        StatementInferenceInner {
            expressions: FrozenMap::from(expressions),
            #[cfg(debug_assertions)]
            scope,
            bindings: bindings.into_boxed_slice(),
            declarations: declarations.into_boxed_slice(),
            extra,
        }
    }

    pub(super) fn finish_function_decorator_inference(mut self) -> FunctionDecoratorInference<'db> {
        self.infer_region();

        let known_decorators = match self.region {
            InferenceRegion::FunctionDecorators(definition) => match definition.kind(self.db()) {
                // basedpython: a trailing lambda's synthetic decorator holds the
                // called expression — its type must not be read as a decoration
                // (a call returning e.g. `staticmethod` would poison the flags)
                DefinitionKind::Function(function)
                    if !function.node(self.module()).is_trailing_lambda =>
                {
                    function.node(self.module()).decorator_list.iter().fold(
                        FunctionDecorators::empty(),
                        |known_decorators, decorator| {
                            known_decorators
                                | FunctionDecorators::from_decorator_type(
                                    self.db(),
                                    self.expression_type(&decorator.expression),
                                )
                        },
                    )
                }
                _ => FunctionDecorators::empty(),
            },
            _ => FunctionDecorators::empty(),
        };

        let Self {
            context,
            expressions,
            bindings,
            called_functions,
            expression_cache: _,
            basedpython_chain_present: _,
            reachability_cache: _,
            declarations: _,
            deferred: _,
            scope: _,
            string_annotations: _,
            unsolved_typevar_calls: _,
            expected_types: _,
            trailing_lambda_return,
            return_types_and_ranges: _,
            fluid_creation: _,
            fluid_timeline: _,
            fluid_adoptions: _,
            collection_use_constraints: _,
            dataclass_field_specifiers: _,
            slice_materialization: _,
            undecorated_type: _,
            discards_dict_key_assignments: _,
            typevar_binding_context: _,
            deferred_state: _,
            index: _,
            region: _,
            cycle_recovery: _,
            qualifiers: _,
            type_expression_flags: _,
        } = self;
        let diagnostics = context.finish();

        FunctionDecoratorInference {
            expression_types: FrozenMap::from(expressions),
            bindings: bindings.into_boxed_slice(),
            called_functions: called_functions
                .into_iter()
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            known_decorators,
            trailing_lambda_return,
            diagnostics,
        }
    }

    pub(super) fn finish_definition(
        mut self,
        definition: Definition<'db>,
    ) -> DefinitionInference<'db> {
        self.infer_region();
        self.finish_inferred_definition(definition)
    }

    fn finish_inferred_definition(self, definition: Definition<'db>) -> DefinitionInference<'db> {
        let Self {
            context,
            expressions,
            qualifiers,
            type_expression_flags,
            fluid_creation,
            fluid_timeline,
            mut fluid_adoptions,
            mut collection_use_constraints,
            string_annotations,
            expected_types,
            trailing_lambda_return: _,
            scope,
            bindings,
            declarations,
            deferred,
            cycle_recovery,
            undecorated_type,
            discards_dict_key_assignments,
            called_functions,
            unsolved_typevar_calls: _,

            // builder only state
            expression_cache: _,
            basedpython_chain_present: _,
            reachability_cache: _,
            dataclass_field_specifiers: _,
            slice_materialization: _,
            typevar_binding_context: _,
            deferred_state: _,
            index: _,
            region: _,
            return_types_and_ranges: _,
        } = self;

        let _ = scope;
        let diagnostics = context.finish();

        let non_undecorated_extra_field_count = usize::from(!string_annotations.is_empty())
            + usize::from(!expected_types.is_empty())
            + usize::from(!collection_use_constraints.is_empty())
            + usize::from(!fluid_adoptions.is_empty())
            + usize::from(fluid_creation.is_some())
            + usize::from(fluid_timeline.is_some())
            + usize::from(!called_functions.is_empty())
            + usize::from(!type_expression_flags.is_empty())
            + usize::from(cycle_recovery.is_some())
            + usize::from(!deferred.is_empty())
            + usize::from(!diagnostics.is_empty())
            + usize::from(discards_dict_key_assignments)
            + usize::from(!qualifiers.is_empty());

        let extra = match (non_undecorated_extra_field_count, undecorated_type) {
            (0, None) => None,
            (1, None) if !qualifiers.is_empty() => Some(Box::new(
                DefinitionInferenceExtra::Qualifiers(FrozenMap::from(qualifiers)),
            )),
            (1, None) if !deferred.is_empty() => Some(Box::new(
                DefinitionInferenceExtra::Deferred(deferred.into_boxed_slice()),
            )),
            (1, None) if !diagnostics.is_empty() => Some(Box::new(
                DefinitionInferenceExtra::Diagnostics(Box::new(diagnostics)),
            )),
            (1, None) if !called_functions.is_empty() => {
                Some(Box::new(DefinitionInferenceExtra::CalledFunctions(
                    called_functions
                        .into_iter()
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                )))
            }
            (1, None) if !expected_types.is_empty() => Some(Box::new(
                DefinitionInferenceExtra::ExpectedTypes(FrozenMap::from(expected_types)),
            )),
            (1, None) if !string_annotations.is_empty() => Some(Box::new(
                DefinitionInferenceExtra::StringAnnotations(FrozenSet::from(string_annotations)),
            )),
            (1, None) if discards_dict_key_assignments => Some(Box::new(
                DefinitionInferenceExtra::DiscardsDictKeyAssignments,
            )),
            (0, Some(undecorated_type)) => Some(Box::new(DefinitionInferenceExtra::Undecorated(
                Box::new(undecorated_type),
            ))),
            (1, Some(undecorated_type)) if !deferred.is_empty() => {
                Some(Box::new(DefinitionInferenceExtra::DeferredAndUndecorated(
                    Box::new(DeferredAndUndecorated {
                        deferred: deferred.into_boxed_slice(),
                        undecorated_type,
                    }),
                )))
            }
            (_, undecorated_type) => {
                collection_use_constraints.shrink_to_fit();
                fluid_adoptions.shrink_to_fit();
                let extra = OtherDefinitionInferenceExtra {
                    string_annotations: FrozenSet::from(string_annotations),
                    expected_types: FrozenMap::from(expected_types),
                    fluid_adoptions,
                    collection_use_constraints,
                    fluid_creation,
                    fluid_timeline,
                    called_functions: called_functions
                        .into_iter()
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                    type_expression_flags: FrozenMap::from(type_expression_flags),
                    cycle_recovery,
                    deferred: deferred.into_boxed_slice(),
                    diagnostics,
                    undecorated_type,
                    discards_dict_key_assignments,
                    qualifiers: FrozenMap::from(qualifiers),
                };
                Some(Box::new(DefinitionInferenceExtra::Other(Box::new(extra))))
            }
        };

        if bindings.len() > 20 {
            tracing::debug!(
                "Inferred definition region `{:?}` contains {} bindings. \
                Lookups by linear scan might be slow.",
                self.region,
                bindings.len(),
            );
        }

        if declarations.len() > 20 {
            tracing::debug!(
                "Inferred declaration region `{:?}` contains {} declarations. \
                Lookups by linear scan might be slow.",
                self.region,
                declarations.len(),
            );
        }

        DefinitionInference {
            expressions: FrozenMap::from(expressions),
            #[cfg(debug_assertions)]
            scope,
            types: DefinitionTypes::from_parts(
                definition,
                bindings.into_vec(),
                declarations.into_vec(),
            ),
            extra,
        }
    }

    pub(super) fn finish_scope(mut self) -> ScopeInference<'db> {
        self.infer_region();

        let Self {
            context,
            string_annotations,
            unsolved_typevar_calls: _,
            expected_types,
            trailing_lambda_return: _,
            type_expression_flags,
            fluid_creation: _,
            fluid_timeline: _,
            mut fluid_adoptions,
            mut collection_use_constraints,
            expressions,
            scope,
            cycle_recovery,
            qualifiers,

            // Ignored, never leaked into other scopes
            deferred: _,
            bindings: _,
            declarations: _,

            // Ignored; only relevant to definition regions
            undecorated_type: _,
            discards_dict_key_assignments: _,

            // Builder only state
            expression_cache: _,
            basedpython_chain_present: _,
            reachability_cache: _,
            dataclass_field_specifiers: _,
            slice_materialization: _,
            typevar_binding_context: _,
            deferred_state: _,
            called_functions: _,
            index: _,
            region: _,
            return_types_and_ranges: _,
        } = self;

        let _ = scope;
        let diagnostics = context.finish();

        let extra = (!string_annotations.is_empty()
            || !expected_types.is_empty()
            || !diagnostics.is_empty()
            || cycle_recovery.is_some()
            || !type_expression_flags.is_empty()
            || !collection_use_constraints.is_empty()
            || !qualifiers.is_empty()
            || !fluid_adoptions.is_empty())
        .then(|| {
            collection_use_constraints.shrink_to_fit();
            fluid_adoptions.shrink_to_fit();
            Box::new(ScopeInferenceExtra {
                string_annotations: FrozenSet::from(string_annotations),
                qualifiers: FrozenMap::from(qualifiers),
                expected_types: FrozenMap::from(expected_types),
                type_expression_flags: FrozenMap::from(type_expression_flags),
                collection_use_constraints,
                fluid_adoptions,
                cycle_recovery,
                diagnostics,
            })
        });

        ScopeInference {
            expressions: FrozenValueMap::from(expressions),
            extra,
        }
    }

    const fn inference_flags(&self) -> InferenceFlags {
        self.context.inference_flags
    }

    /// Returns a fresh [`TypeInferenceBuilder`] for the current scope that can be used
    /// to speculatively infer expressions during multi-inference.
    ///
    /// The inference results can be merged into the current inference region using
    /// [`TypeInferenceBuilder::extend`].
    fn speculate(&self) -> Self {
        let db = self.db();
        let Self {
            region,
            index,
            cycle_recovery,
            deferred_state,
            typevar_binding_context,
            ref expression_cache,
            ref reachability_cache,
            ref return_types_and_ranges,
            ref dataclass_field_specifiers,
            slice_materialization: _,

            // These fields are type inference results, but do not affect the inference of a given
            // expression.
            context: _,
            fluid_creation: _,
            fluid_timeline: _,
            fluid_adoptions: _,
            basedpython_chain_present: _,
            collection_use_constraints: _,
            expressions: _,
            string_annotations: _,
            unsolved_typevar_calls: _,
            expected_types: _,
            trailing_lambda_return: _,
            scope: _,
            bindings: _,
            declarations: _,
            deferred: _,
            called_functions: _,
            undecorated_type: _,
            discards_dict_key_assignments: _,
            qualifiers: _,
            type_expression_flags: _,
        } = *self;

        let mut builder = TypeInferenceBuilder::new(
            db,
            self.program_environment(),
            region,
            self.file(),
            self.program_file(),
            index,
            self.module(),
        );

        // Speculated builders are often discarded immediately.
        builder.context.defuse();

        // Ensure the speculative builder has the same inference context as the current one.
        builder.cycle_recovery = cycle_recovery;
        builder.deferred_state = deferred_state;
        builder.typevar_binding_context = typevar_binding_context;
        builder.context.inference_flags = self.inference_flags();
        builder.expression_cache.clone_from(expression_cache);
        builder.reachability_cache.clone_from(reachability_cache);
        builder
            .return_types_and_ranges
            .clone_from(return_types_and_ranges);
        builder
            .dataclass_field_specifiers
            .clone_from(dataclass_field_specifiers);

        builder
    }

    /// Returns a speculative builder that does not construct diagnostics.
    ///
    /// Note that this method may lead to lost diagnostics if the expression cache
    /// is enabled, as future multi-inference attempts may reuse inference results
    /// in which diagnostics were suppressed.
    fn speculate_without_diagnostics(&self) -> Self {
        let mut builder = self.speculate();
        builder.context.suppress_diagnostics();
        builder
    }

    /// Extend the current region with the results of a speculative [`TypeInferenceBuilder`].
    fn extend(&mut self, other: Self) {
        let Self {
            context,
            expressions,
            type_expression_flags,
            fluid_creation: _,
            fluid_timeline: _,
            fluid_adoptions,
            collection_use_constraints,
            string_annotations,
            unsolved_typevar_calls,
            expected_types,
            trailing_lambda_return: _,
            scope,
            bindings,
            declarations,
            deferred,
            cycle_recovery,
            dataclass_field_specifiers: _,
            slice_materialization: _,

            // Ignored; only relevant to definition regions
            undecorated_type: _,
            discards_dict_key_assignments: _,

            // builder only state
            expression_cache: _,
            basedpython_chain_present,
            reachability_cache: _,
            typevar_binding_context: _,
            deferred_state: _,
            called_functions,
            index: _,
            region: _,
            return_types_and_ranges: _,
            qualifiers: _,
        } = other;

        let diagnostics = context.finish();
        let _ = scope;

        assert!(
            declarations.is_empty(),
            "speculative `TypeInferenceBuilder` should only be used for expression inference"
        );
        assert!(
            deferred.is_empty(),
            "speculative `TypeInferenceBuilder` should only be used for expression inference"
        );

        self.expressions.extend(expressions.iter());
        self.context.extend(&diagnostics);
        self.extend_cycle_recovery(cycle_recovery);
        self.string_annotations
            .extend(string_annotations.iter().copied());
        self.unsolved_typevar_calls
            .extend(unsolved_typevar_calls.iter().copied());
        self.expected_types.extend(expected_types.iter());
        self.type_expression_flags
            .extend(type_expression_flags.iter());
        self.called_functions.extend(called_functions);

        if !matches!(self.region, InferenceRegion::Scope(..)) {
            self.bindings
                .extend(bindings.iter().map(|(def, ty)| (*def, *ty)));
        }

        self.fluid_adoptions.extend(fluid_adoptions);

        // adopting the speculative builder's expression types means adopting the optional-chain
        // provenance of those same expressions, or a chain this region goes on to extend would
        // resolve its next link against a short-circuit `None`
        self.basedpython_chain_present
            .extend(basedpython_chain_present);

        #[expect(
            clippy::iter_over_hash_type,
            reason = "constraints for distinct collection definitions are merged independently"
        )]
        for (collection_def, constraints) in &collection_use_constraints {
            self.collection_use_constraints
                .entry(*collection_def)
                .and_modify(|this| this.extend(constraints))
                .or_insert(constraints.clone());
        }
    }
}

/// An expression cache shared across builders during multi-inference.
///
/// This provides a cheap way of reusing inference results without the overhead
/// of Salsa standalone expressions.
#[derive(Default)]
struct ExpressionCache<'db> {
    entries: FxHashMap<ExpressionNodeKey, ExpressionCacheEntries<'db>>,
}

impl<'db> ExpressionCache<'db> {
    fn get(
        &self,
        expression: ExpressionNodeKey,
        tcx: TypeContext<'db>,
    ) -> Option<&ExpressionCacheEntry<'db>> {
        self.entries.get(&expression)?.get(tcx)
    }

    fn insert(
        &mut self,
        expression: ExpressionNodeKey,
        tcx: TypeContext<'db>,
        value: ExpressionCacheEntry<'db>,
    ) {
        match self.entries.entry(expression) {
            hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().insert(tcx, value);
            }
            hash_map::Entry::Vacant(entry) => {
                entry.insert(ExpressionCacheEntries::Single(tcx, value));
            }
        }
    }
}

/// The inferred types of a given expression, keyed by type context.
enum ExpressionCacheEntries<'db> {
    Single(TypeContext<'db>, ExpressionCacheEntry<'db>),
    Many(FxHashMap<TypeContext<'db>, ExpressionCacheEntry<'db>>),
}

impl<'db> ExpressionCacheEntries<'db> {
    fn get(&self, tcx: TypeContext<'db>) -> Option<&ExpressionCacheEntry<'db>> {
        match self {
            Self::Single(cached_tcx, value) if *cached_tcx == tcx => Some(value),
            Self::Single(_, _) => None,
            Self::Many(values) => values.get(&tcx),
        }
    }

    fn insert(&mut self, tcx: TypeContext<'db>, value: ExpressionCacheEntry<'db>) {
        if let Self::Single(cached_tcx, cached_value) = self
            && *cached_tcx == tcx
        {
            *cached_value = value;
            return;
        }

        let previous = std::mem::replace(self, Self::Many(FxHashMap::default()));
        *self = match previous {
            Self::Single(cached_tcx, cached_value) => Self::Many(FxHashMap::from_iter([
                (cached_tcx, cached_value),
                (tcx, value),
            ])),
            Self::Many(mut values) => {
                values.insert(tcx, value);
                Self::Many(values)
            }
        };
    }
}

/// The inferred types for an expression region under a given type context.
#[derive(Clone)]
enum ExpressionCacheEntry<'db> {
    Small(Type<'db>),
    Full(Rc<FullExpressionCacheEntry<'db>>),
}

/// The full inference results for an expression region.
///
/// Unlike [`ExpressionInference`], this type is short-lived, and avoids the cost of compaction
/// that is otherwise performed for Salsa results.
struct FullExpressionCacheEntry<'db> {
    expressions: FxHashMap<ExpressionNodeKey, Type<'db>>,
    type_expression_flags: FxHashMap<ExpressionNodeKey, TypeExpressionFlags>,
    collection_use_constraints: CollectionUseConstraints<'db>,
    fluid_adoptions: FxHashMap<ExpressionNodeKey, Type<'db>>,
    fluid_creation: Option<Type<'db>>,
    fluid_timeline: Option<FluidTimeline<'db>>,
    string_annotations: FxHashSet<ExpressionNodeKey>,
    unsolved_typevar_calls: FxHashSet<ExpressionNodeKey>,
    expected_types: FxHashMap<ExpressionNodeKey, Type<'db>>,
    bindings: VecMap<Definition<'db>, Type<'db>>,
    diagnostics: TypeCheckDiagnostics,
    called_functions: FxIndexSet<FunctionType<'db>>,
    cycle_recovery: Option<Type<'db>>,
    #[cfg(debug_assertions)]
    scope: ScopeId<'db>,
}

impl<'db> FullExpressionCacheEntry<'db> {
    fn expression_type(&self, expression: ExpressionNodeKey) -> Type<'db> {
        self.expressions
            .get(&expression)
            .copied()
            .or(self.cycle_recovery)
            .unwrap_or_else(Type::unknown)
    }

    fn is_single_expression(&self, expression: ExpressionNodeKey, ty: Type<'db>) -> bool {
        self.expressions.len() == 1
            && self.expressions.get(&expression) == Some(&ty)
            && self.type_expression_flags.is_empty()
            && self.collection_use_constraints.is_empty()
            && self.fluid_adoptions.is_empty()
            && self.fluid_creation.is_none()
            && self.fluid_timeline.is_none()
            && self.string_annotations.is_empty()
            && self.unsolved_typevar_calls.is_empty()
            && self.expected_types.is_empty()
            && self.bindings.is_empty()
            && self.diagnostics.is_empty()
            && self.called_functions.is_empty()
            && self.cycle_recovery.is_none()
    }

    fn into_expression_inference(
        mut self,
        region: InferenceRegion<'db>,
    ) -> ExpressionInference<'db> {
        let extra = (!self.string_annotations.is_empty()
            || !self.unsolved_typevar_calls.is_empty()
            || !self.type_expression_flags.is_empty()
            || !self.collection_use_constraints.is_empty()
            || !self.fluid_adoptions.is_empty()
            || self.fluid_creation.is_some()
            || self.fluid_timeline.is_some()
            || !self.expected_types.is_empty()
            || self.cycle_recovery.is_some()
            || !self.bindings.is_empty()
            || !self.called_functions.is_empty()
            || !self.diagnostics.is_empty())
        .then(|| {
            if self.bindings.len() > 20 {
                tracing::debug!(
                    "Inferred expression region `{:?}` contains {} bindings. \
                    Lookups by linear scan might be slow.",
                    region,
                    self.bindings.len()
                );
            }

            self.collection_use_constraints.shrink_to_fit();
            self.fluid_adoptions.shrink_to_fit();
            self.diagnostics.shrink_to_fit();
            Box::new(ExpressionInferenceExtra {
                string_annotations: FrozenSet::from(self.string_annotations),
                unsolved_typevar_calls: FrozenSet::from(self.unsolved_typevar_calls),
                fluid_adoptions: self.fluid_adoptions,
                fluid_creation: self.fluid_creation,
                fluid_timeline: self.fluid_timeline,
                expected_types: FrozenMap::from(self.expected_types),
                type_expression_flags: FrozenMap::from(self.type_expression_flags),
                bindings: self.bindings.into_boxed_slice(),
                diagnostics: self.diagnostics,
                called_functions: self.called_functions.into_iter().collect(),
                cycle_recovery: self.cycle_recovery,
                collection_use_constraints: self.collection_use_constraints,
            })
        });

        ExpressionInference {
            expressions: FrozenMap::from(self.expressions),
            extra,
            #[cfg(debug_assertions)]
            scope: self.scope,
        }
    }
}

/// Manages the inference of a given expression.
struct MultiInferenceGuard<'db, 'ast, 'infer> {
    infer_expr:
        &'infer mut dyn FnMut(&mut TypeInferenceBuilder<'db, 'ast>, TypeContext<'db>) -> Type<'db>,
    last_tcx: Option<TypeContext<'db>>,
    finalized: bool,
}

impl<'db, 'ast, 'infer> MultiInferenceGuard<'db, 'ast, 'infer> {
    /// Creates a [`MultiInferenceGuard`] for the given expression.
    fn new(
        infer_expr: &'infer mut dyn FnMut(
            &mut TypeInferenceBuilder<'db, 'ast>,
            TypeContext<'db>,
        ) -> Type<'db>,
    ) -> Self {
        Self {
            infer_expr,
            last_tcx: None,
            finalized: false,
        }
    }

    /// Infer the expression with diagnostics enabled.
    ///
    /// This method must be called exactly once in the lifetime of the [`MultiInferenceGuard`].
    fn infer_loud(
        &mut self,
        builder: &mut TypeInferenceBuilder<'db, 'ast>,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        debug_assert!(
            !self.finalized,
            "called `infer_loud` multiple times on a `MultiInferenceGuard`"
        );

        self.finalized = true;
        (self.infer_expr)(builder, tcx)
    }

    /// Infer the expression silently, with diagnostics disabled.
    ///
    /// This method may be called an unlimited number of times.
    fn infer_silent(
        &mut self,
        builder: &mut TypeInferenceBuilder<'db, 'ast>,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        self.last_tcx = Some(tcx);
        (self.infer_expr)(&mut builder.speculate_without_diagnostics(), tcx)
    }

    fn last_tcx(&self) -> TypeContext<'db> {
        self.last_tcx.unwrap_or_default()
    }
}

impl Drop for MultiInferenceGuard<'_, '_, '_> {
    fn drop(&mut self) {
        debug_assert!(
            self.finalized,
            "dropped `MultiInferenceGuard` without calling `infer_loud`"
        );
    }
}

/// An expression representing the function argument at the given index, along with its type
/// context.
type ArgExpr<'db, 'ast> = (usize, &'ast ast::Expr, TypeContext<'db>);

/// basedpython: the owner of a type-parameter list, as far as reification is
/// concerned.
///
/// Reification rebuilds the *function's* closure so its body sees the type
/// argument as a value; nothing else has such a step, so a `reified` parameter
/// declared elsewhere promises a runtime value that never arrives.
#[derive(Clone, Copy)]
pub(super) enum TypeParamReification {
    Function,
    Class,
    TypeAlias,
    /// a `type def`, whose declaration the transpiler erases entirely
    TypeDef,
}

impl TypeParamReification {
    /// Why `type_param` cannot be reified on this owner, or `None` when it can.
    ///
    /// This mirrors what [`crate::reified::reified_type_param_names`] honours,
    /// so the modifier is reported exactly when it would otherwise be silently
    /// dropped.
    fn rejection(
        self,
        type_param: &ast::TypeParam,
        source_type: PySourceType,
    ) -> Option<&'static str> {
        match self {
            Self::Function => (type_param.is_param_spec()
                && !matches!(source_type, PySourceType::BasedPython))
            .then_some(
                "outside a `.by` source file `**P` declares a PEP 612 `ParamSpec`, and a \
                 parameter list has no runtime object to bind",
            ),
            Self::Class => {
                Some("a class's type parameters are erased; only a function reifies one")
            }
            Self::TypeAlias => {
                Some("a type alias's type parameters are erased; only a function reifies one")
            }
            Self::TypeDef => Some(
                "a `type def` is erased by the transpiler, so its type parameters have no \
                 runtime value",
            ),
        }
    }

    /// Why a variance keyword on this owner's type parameter decides nothing, or
    /// `None` when it decides something.
    ///
    /// Variance relates two *specializations*. A function's type parameter is
    /// solved afresh at each call and never specializes, so the keyword decides
    /// nothing there; an alias does specialize, taking its variance from the
    /// type it expands to.
    fn variance_rejection(self) -> Option<&'static str> {
        match self {
            // an alias keeps the keyword: it is exactly as variant as the type it expands
            // to, so writing the variance out is a statement about that expansion
            Self::Class | Self::TypeAlias => None,
            Self::Function => Some(
                "a function's type parameter is solved afresh at each call, so no two uses of \
                 it are related by variance",
            ),
            Self::TypeDef => Some(
                "a `type def` is erased by the transpiler, so nothing observes its type \
                 parameters",
            ),
        }
    }
}

#[derive(Clone, Copy)]
enum CallArgumentInferenceMode {
    /// Infer against every candidate type context entirely speculatively.
    Speculate,

    /// Commit a default inference without type context, if there are multiple
    /// applicable type contexts.
    Commit,
}

impl CallArgumentInferenceMode {
    fn requires_default_inference(self) -> bool {
        matches!(self, Self::Commit)
    }
}

/// The set of type contexts to use when inferring a call-site argument, across all matching overloads.
#[derive(Debug, PartialEq, Eq)]
enum MatchingArgumentTypeContext<'db> {
    Unique(Option<ArgumentTypeContext<'db>>),
    Many(Vec<Option<ArgumentTypeContext<'db>>>),
}

fn is_collection_literal(expression: &ast::Expr) -> bool {
    matches!(
        expression,
        ast::Expr::List(_) | ast::Expr::Set(_) | ast::Expr::Dict(_)
    )
}

/// the flat arms of a `|` union type expression (`A | B | C` → `[A, B, C]`), or
/// `None` when `expr` is not a union — used to test each arm of a parametric
/// `is`-target union independently
/// basedpython: how a keyword-form `is`/`is not` instance check is decided, which
/// is what the `non-overlapping-type-test` lint reads to know whether the test can
/// ever hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IsTestDecision {
    /// a bare class target (`x is int`): ordinary disjointness decides it
    Instance,
    /// a parametric target (`x is list[int]`) the parametric engine folded to
    /// `False`. That engine is asked rather than disjointness directly because it
    /// also honours a use-site variance projection (`a is A[out int]`)
    ParametricNeverHolds,
    /// a parametric target left to a runtime probe, or a union with such an arm —
    /// nothing the lint can call constant
    Undecided,
}

fn union_target_arms(expr: &ast::Expr) -> Option<Vec<&ast::Expr>> {
    fn collect<'a>(expr: &'a ast::Expr, arms: &mut Vec<&'a ast::Expr>) {
        if let ast::Expr::BinOp(binop) = expr
            && binop.op == ast::Operator::BitOr
        {
            collect(&binop.left, arms);
            collect(&binop.right, arms);
        } else {
            arms.push(expr);
        }
    }
    if !matches!(expr, ast::Expr::BinOp(binop) if binop.op == ast::Operator::BitOr) {
        return None;
    }
    let mut arms = Vec::new();
    collect(expr, &mut arms);
    Some(arms)
}

/// Returns `true` if `expression` is a link of a basedpython optional chain: a `?.` access, or a
/// trailer applied to one.
///
/// A chain runs from its first `?.` out through the trailers that follow it, exactly as far as the
/// `None if a is None else <rest of chain>` lowering short-circuits. `a?.b.c()[0]` is one chain of
/// four links, so an absent `a` skips all of `.b`, `.c`, `()` and `[0]`.
fn is_basedpython_chain_link(expression: &ast::Expr) -> bool {
    match expression {
        ast::Expr::Attribute(attribute) => {
            attribute.optional || is_basedpython_chain_link(&attribute.value)
        }
        ast::Expr::Call(call) => is_basedpython_chain_link(&call.func),
        ast::Expr::Subscript(subscript) => is_basedpython_chain_link(&subscript.value),
        _ => false,
    }
}

/// Returns `true` if `tcx` cannot provide useful type context for a collection literal.
///
/// During generic call argument inference, type variables that cannot yet be specialized are
/// replaced by `UnspecializedTypeVar`. This marker intentionally carries neither type-variable
/// identity nor a concrete expected type, and collection literal inference ignores it rather than
/// using it as a constraint.
///
/// A bare generic parameter, such as the parameter to `reveal_type`, therefore provides an exact
/// `UnspecializedTypeVar` context that should not prevent a peer expression from providing context
/// instead.
///
/// This deliberately matches only the bare marker: a partially specialized context such as
/// `list[UnspecializedTypeVar | int]` still carries useful collection structure and concrete type
/// information.
fn is_empty_collection_type_context(tcx: TypeContext<'_>) -> bool {
    tcx.annotation()
        .is_none_or(|annotation| annotation == Type::Dynamic(DynamicType::UnspecializedTypeVar))
}

/// An iterator over arguments to a functional call.
#[derive(Clone)]
enum ArgumentsIter<'a> {
    FromAst(ArgumentsSourceOrder<'a>),
    Synthesized(std::slice::Iter<'a, ArgOrKeyword<'a>>),
}

impl<'a> ArgumentsIter<'a> {
    fn from_ast(arguments: &'a ast::Arguments) -> Self {
        Self::FromAst(arguments.iter_source_order())
    }

    fn synthesized(arguments: &'a [ArgOrKeyword<'a>]) -> Self {
        Self::Synthesized(arguments.iter())
    }
}

impl<'a> Iterator for ArgumentsIter<'a> {
    type Item = ArgOrKeyword<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            ArgumentsIter::FromAst(args) => args.next(),
            ArgumentsIter::Synthesized(args) => args.next().copied(),
        }
    }
}

/// The deferred state of a specific expression in an inference region.
#[derive(Default, Debug, Clone, Copy)]
enum DeferredExpressionState {
    /// The expression is not deferred.
    #[default]
    None,

    /// The expression is deferred.
    ///
    /// In the following example,
    /// ```py
    /// from __future__ import annotation
    ///
    /// a: tuple[int, "ForwardRef"] = ...
    /// ```
    ///
    /// The expression `tuple` and `int` are deferred but `ForwardRef` (after parsing) is both
    /// deferred and in a string annotation context.
    Deferred,

    /// The expression is in a string annotation context.
    ///
    /// This is required to differentiate between a deferred annotation and a string annotation.
    /// The former can occur when there's a `from __future__ import annotations` statement or we're
    /// in a stub file.
    ///
    /// In the following example,
    /// ```py
    /// a: "List[int]" = ...
    /// b: tuple[int, "ForwardRef"] = ...
    /// ```
    ///
    /// The annotation of `a` is completely inside a string while for `b`, it's only partially
    /// stringified.
    ///
    /// This variant wraps a [`NodeKey`] that allows us to retrieve the original
    /// [`ast::ExprStringLiteral`] node which created the string annotation.
    InStringAnnotation(NodeKey),
}

impl DeferredExpressionState {
    const fn is_deferred(self) -> bool {
        matches!(
            self,
            DeferredExpressionState::Deferred | DeferredExpressionState::InStringAnnotation(_)
        )
    }

    const fn in_string_annotation(self) -> bool {
        matches!(self, DeferredExpressionState::InStringAnnotation(_))
    }
}

impl From<bool> for DeferredExpressionState {
    fn from(value: bool) -> Self {
        if value {
            DeferredExpressionState::Deferred
        } else {
            DeferredExpressionState::None
        }
    }
}

/// Struct collecting string parts when inferring a formatted string. Infers a string literal if the
/// concatenated string is small enough, otherwise infers a literal string.
///
/// If the formatted string contains an expression (with a representation unknown at compile time),
/// infers an instance of `builtins.str`.
#[derive(Debug)]
struct StringPartsCollector {
    concatenated: Option<CompactString>,
    contains_non_literal_str: bool,
}

impl StringPartsCollector {
    fn new() -> Self {
        Self {
            concatenated: Some(CompactString::new("")),
            contains_non_literal_str: false,
        }
    }

    fn push_str(&mut self, literal: &str) {
        if let Some(mut concatenated) = self.concatenated.take() {
            if concatenated.len().saturating_add(literal.len())
                <= TypeInferenceBuilder::MAX_STRING_LITERAL_SIZE
            {
                concatenated.push_str(literal);
                self.concatenated = Some(concatenated);
            } else {
                self.concatenated = None;
            }
        }
    }

    /// Add an expression whose `__str__` return type is `LiteralString`.
    /// The exact value is unknown, so we can't track the concatenated string,
    /// but the result is still `LiteralString`.
    fn add_literal_string_expression(&mut self) {
        self.concatenated = None;
    }

    /// Add an expression whose `__str__` return type is not `LiteralString`.
    /// The result will degrade to `str`.
    fn add_non_literal_string_expression(&mut self) {
        self.concatenated = None;
        self.contains_non_literal_str = true;
    }

    fn string_type<'db>(self, context: &InferContext<'db, '_>) -> Type<'db> {
        let db = context.db();
        if self.contains_non_literal_str {
            KnownClass::Str.to_instance(db, context.program_environment())
        } else if let Some(concatenated) = self.concatenated {
            Type::string_literal(db, &concatenated)
        } else {
            Type::LiteralValue(LiteralValueType::promotable(
                LiteralValueTypeKind::LiteralString,
            ))
        }
    }
}

/// Map based on a `Vec`. It doesn't enforce
/// uniqueness on insertion. Instead, it relies on the caller
/// that elements are unique. For example, the way we visit definitions
/// in the `TypeInference` builder already implicitly guarantees that each definition
/// is only visited once.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VecMap<K, V>(Vec<(K, V)>);

impl<K, V> VecMap<K, V> {
    #[inline]
    fn len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn iter(&self) -> VecMapIterator<'_, K, V> {
        VecMapIterator {
            inner: self.0.iter(),
        }
    }

    fn into_boxed_slice(self) -> Box<[(K, V)]> {
        self.0.into_boxed_slice()
    }

    fn into_vec(self) -> Vec<(K, V)> {
        self.0
    }
}

impl<K, V> VecMap<K, V>
where
    K: Eq,
    K: std::fmt::Debug,
    V: std::fmt::Debug,
{
    fn insert(&mut self, key: K, value: V) {
        debug_assert!(
            !self.0.iter().any(|(existing, _)| existing == &key),
            "An existing entry already exists for key {key:?}",
        );

        self.0.push((key, value));
    }

    #[inline]
    fn extend<T: IntoIterator<Item = (K, V)>>(&mut self, iter: T) {
        if cfg!(debug_assertions) {
            for (key, value) in iter {
                self.insert(key, value);
            }
        } else {
            self.0.extend(iter);
        }
    }
}

impl<K, V> Default for VecMap<K, V> {
    fn default() -> Self {
        Self(Vec::default())
    }
}

impl<'a, K, V> IntoIterator for &'a VecMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = VecMapIterator<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

struct VecMapIterator<'a, K, V> {
    inner: std::slice::Iter<'a, (K, V)>,
}

impl<'a, K, V> Iterator for VecMapIterator<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(k, v)| (k, v))
    }
}

impl<K, V> std::iter::FusedIterator for VecMapIterator<'_, K, V> {}

impl<K, V> ExactSizeIterator for VecMapIterator<'_, K, V> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

/// Set based on a `Vec`. It doesn't enforce
/// uniqueness on insertion. Instead, it relies on the caller
/// that elements are unique. For example, the way we visit definitions
/// in the `TypeInference` builder make already implicitly guarantees that each definition
/// is only visited once.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VecSet<V>(Vec<V>);

impl<V> VecSet<V> {
    #[inline]
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn into_boxed_slice(self) -> Box<[V]> {
        self.0.into_boxed_slice()
    }
}

impl<V> VecSet<V>
where
    V: Eq,
    V: std::fmt::Debug,
{
    fn insert(&mut self, value: V) {
        debug_assert!(
            !self.0.iter().any(|existing| existing == &value),
            "An existing entry already exists for {value:?}",
        );

        self.0.push(value);
    }

    #[inline]
    fn extend<T: IntoIterator<Item = V>>(&mut self, iter: T) {
        if cfg!(debug_assertions) {
            for value in iter {
                self.insert(value);
            }
        } else {
            self.0.extend(iter);
        }
    }
}

impl<V> Default for VecSet<V> {
    fn default() -> Self {
        Self(Vec::default())
    }
}

impl<V> IntoIterator for VecSet<V> {
    type Item = V;
    type IntoIter = std::vec::IntoIter<V>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

#[must_use]
struct AddBinding<'db, 'ast> {
    declared_ty: Option<Type<'db>>,
    binding: Definition<'db>,
    node: AnyNodeRef<'ast>,
    qualifiers: TypeQualifiers,
    is_local: bool,
}

impl<'db, 'ast> AddBinding<'db, 'ast> {
    fn type_context(&self) -> TypeContext<'db> {
        TypeContext::new(self.declared_ty)
    }

    fn insert(
        self,
        builder: &mut TypeInferenceBuilder<'db, 'ast>,
        inferred_ty: Type<'db>,
    ) -> Type<'db> {
        let env = builder.program_environment();
        let declared_ty = self.declared_ty.unwrap_or(Type::unknown());

        let db = builder.db();
        let file_scope_id = self.binding.file_scope(db);
        let use_def = builder.index.use_def_map(file_scope_id);
        let place_table = builder.index.place_table(file_scope_id);

        let mut bound_ty = inferred_ty;

        if self.qualifiers.contains(TypeQualifiers::FINAL) {
            let mut previous_bindings = use_def.bindings_at_definition(self.binding);

            // An assignment to a local `Final`-qualified symbol is only an error if there are prior bindings

            let previous_definition = previous_bindings.find_map(|r| r.binding.definition());

            if !self.is_local || previous_definition.is_some() {
                let place = place_table.place(self.binding.place(db));
                if let Some(diag_builder) = builder.context.report_lint(
                    &INVALID_ASSIGNMENT,
                    self.binding.full_range(builder.db(), builder.module()),
                ) {
                    let mut diagnostic = diag_builder.into_diagnostic(format_args!(
                        "Reassignment of `Final` symbol `{place}` is not allowed"
                    ));

                    diagnostic.set_primary_annotation_message("Reassignment of `Final` symbol");

                    if let Some(previous_definition) = previous_definition {
                        // It is not very helpful to show the previous definition if it results from
                        // an import. Ideally, we would show the original definition in the external
                        // module, but that information is currently not threaded through attribute
                        // lookup.
                        if !previous_definition.kind(db).is_import() {
                            if let DefinitionKind::AnnotatedAssignment(assignment) =
                                previous_definition.kind(db)
                            {
                                let range = assignment.annotation(builder.module()).range();
                                diagnostic.annotate(
                                    builder
                                        .context
                                        .secondary(range)
                                        .message("Symbol declared as `Final` here"),
                                );
                            } else {
                                let range = previous_definition.full_range(db, builder.module());
                                diagnostic.annotate(
                                    builder
                                        .context
                                        .secondary(range)
                                        .message("Symbol declared as `Final` here"),
                                );
                            }
                            diagnostic
                                .set_primary_annotation_message("Symbol later reassigned here");
                        }
                    }
                }
            }
        }

        if bound_ty.is_assignable_to(db, env, declared_ty) {
            report_bool_as_int_assignment(
                &builder.context,
                self.node,
                self.binding,
                declared_ty,
                bound_ty,
            );
        } else {
            builder.discard_dict_key_assignments_for(self.binding);
            report_invalid_assignment(
                &builder.context,
                self.node,
                self.binding,
                declared_ty,
                bound_ty,
            );

            // Allow declarations to override inference in case of invalid assignment.
            bound_ty = declared_ty;
        }
        // In the following cases, the bound type may not be the same as the RHS value type.
        if let AnyNodeRef::ExprAttribute(ast::ExprAttribute { value, attr, .. }) = self.node {
            let value_ty = builder.try_expression_type(value).unwrap_or_else(|| {
                builder.infer_maybe_standalone_expression(value, TypeContext::default())
            });
            // If the member is a data descriptor, the RHS value may differ from the value actually assigned.
            if assignment_attribute_members(db, env, value_ty, &attr.id)
                .and_then(AssignmentAttributeMembers::type_member)
                .and_then(|member| member.place.ignore_possibly_undefined())
                .is_some_and(|ty| ty.may_be_data_descriptor(db, env))
            {
                builder.discard_dict_key_assignments_for(self.binding);
                bound_ty = declared_ty;
            }
        } else if let AnyNodeRef::ExprSubscript(ast::ExprSubscript { value, .. }) = self.node {
            let value_ty = builder
                .try_expression_type(value)
                .unwrap_or_else(|| builder.infer_expression(value, TypeContext::default()));

            if !value_ty.is_typed_dict() && !Self::is_safe_mutable_class(db, env, value_ty) {
                builder.discard_dict_key_assignments_for(self.binding);
                bound_ty = declared_ty;
            }
        }

        builder.bindings.insert(self.binding, bound_ty);

        inferred_ty
    }

    /// Arbitrary `__getitem__`/`__setitem__` methods on a class do not
    /// necessarily guarantee that the passed-in value for `__setitem__` is stored and
    /// can be retrieved unmodified via `__getitem__`. Therefore, we currently only
    /// perform assignment-based narrowing on a few built-in classes (`list`, `dict`,
    /// `bytesarray`, `TypedDict`, and `collections` types) where we are confident that
    /// this kind of narrowing can be performed soundly. This is the same approach as
    /// pyright. TODO: Other standard library classes may also be considered safe. Also,
    /// subclasses of these safe classes that do not override `__getitem__/__setitem__`
    /// may be considered safe.
    fn is_safe_mutable_class(
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        ty: Type<'db>,
    ) -> bool {
        const SAFE_MUTABLE_CLASSES: &[KnownClass] = &[
            KnownClass::List,
            KnownClass::Dict,
            KnownClass::Bytearray,
            KnownClass::DefaultDict,
            KnownClass::ChainMap,
            KnownClass::Counter,
            KnownClass::Deque,
            KnownClass::OrderedDict,
        ];

        SAFE_MUTABLE_CLASSES
            .iter()
            .map(|class| class.to_instance(db, env))
            .any(|safe_mutable_class| {
                ty.is_equivalent_to(db, env, safe_mutable_class)
                    || ty
                        .generic_origin(db, env)
                        .zip(safe_mutable_class.generic_origin(db, env))
                        .is_some_and(|(l, r)| l == r)
            })
    }
}

#[derive(Copy, Clone, Debug)]
enum BoundOrConstraintsNodes<'ast> {
    Bound(&'ast ast::Expr),
    Constraints(&'ast [ast::Expr]),
}

/// basedpython: returns `true` if `object_ty` is an instance whose
/// specialization has at least one `out` (covariant) projection on a typevar
/// AND the named `attribute` on the unspecialized class declares its type
/// referring to that very typevar (directly or nested). Under an `out`
/// projection the typevar's contravariant occurrences materialize to
/// `Never`, so any value assignment fails.
fn attribute_has_covariant_projected_typevar<'db>(
    db: &'db dyn crate::Db,
    env: &ProgramEnvironment<'db>,
    object_ty: Type<'db>,
    attribute: &str,
) -> bool {
    use ruff_python_ast::helpers::UseSiteVariance;

    let Some(instance) = object_ty.as_nominal_instance() else {
        return false;
    };
    let crate::types::ClassType::Generic(alias) = instance.class(db, env) else {
        return false;
    };
    let specialization = alias.specialization(db);
    let projections = specialization.projections(db);
    if projections.is_empty() {
        return false;
    }

    let class_literal = alias.origin(db);
    let Some(generic_context) = class_literal.generic_context(db) else {
        return false;
    };

    // Collect identities of typevars that are covariantly projected at use site.
    let covariant_typevar_indices: Vec<usize> = projections
        .iter()
        .enumerate()
        .filter_map(|(i, p)| matches!(p, Some(UseSiteVariance::Out)).then_some(i))
        .collect();
    if covariant_typevar_indices.is_empty() {
        return false;
    }

    let typevars: Vec<_> = generic_context.variables(db).collect();
    let target_typevar_identities: Vec<_> = covariant_typevar_indices
        .iter()
        .filter_map(|i| typevars.get(*i).map(|tv| tv.identity(db)))
        .collect();

    // Look up the attribute's declared type on the unspecialized class. If
    // the declared type contains any of the covariantly-projected typevars,
    // the assignment must be rejected.
    // Look up the attribute on the unspecialized identity specialization so
    // typevar references in the field declaration are preserved.
    let identity_class_type = class_literal.identity_specialization(db);
    let unspecialized_member = identity_class_type
        .instance_member(db, env, attribute)
        .place
        .ignore_possibly_undefined();
    let Some(declared_ty) = unspecialized_member else {
        return false;
    };

    crate::types::any_over_type(db, env, declared_ty, false, |ty| {
        if let Type::TypeVar(typevar) = ty {
            let identity = typevar.identity(db);
            target_typevar_identities.contains(&identity)
        } else {
            false
        }
    })
}

/// basedpython: the names a match type's `case` pattern captures, in source order.
///
/// A starred capture (`*Rest`) counts the same as a plain one: both introduce exactly one
/// type variable, and both have to be bound consistently across an or-pattern's alternatives.
fn match_type_pattern_captures(
    pattern: &ast::Pattern,
) -> Box<dyn Iterator<Item = &ast::Identifier> + '_> {
    match pattern {
        ast::Pattern::MatchAs(ast::PatternMatchAs {
            pattern: inner,
            name,
            ..
        }) => Box::new(
            inner
                .as_deref()
                .into_iter()
                .flat_map(match_type_pattern_captures)
                .chain(name.as_ref()),
        ),
        ast::Pattern::MatchStar(ast::PatternMatchStar { name, .. }) => {
            Box::new(name.as_ref().into_iter())
        }
        ast::Pattern::MatchSequence(ast::PatternMatchSequence { patterns, .. })
        | ast::Pattern::MatchOr(ast::PatternMatchOr { patterns, .. })
        | ast::Pattern::MatchAnd(ast::PatternMatchAnd { patterns, .. }) => {
            Box::new(patterns.iter().flat_map(match_type_pattern_captures))
        }
        ast::Pattern::MatchValue(_)
        | ast::Pattern::MatchSingleton(_)
        | ast::Pattern::MatchClass(_)
        | ast::Pattern::MatchMapping(_) => Box::new(std::iter::empty()),
    }
}

/// basedpython: the patterns nested directly inside `pattern`.
fn match_type_subpatterns(pattern: &ast::Pattern) -> Box<dyn Iterator<Item = &ast::Pattern> + '_> {
    match pattern {
        ast::Pattern::MatchAs(ast::PatternMatchAs { pattern, .. }) => {
            Box::new(pattern.as_deref().into_iter())
        }
        ast::Pattern::MatchSequence(ast::PatternMatchSequence { patterns, .. })
        | ast::Pattern::MatchOr(ast::PatternMatchOr { patterns, .. })
        | ast::Pattern::MatchAnd(ast::PatternMatchAnd { patterns, .. }) => {
            Box::new(patterns.iter())
        }
        ast::Pattern::MatchStar(_)
        | ast::Pattern::MatchValue(_)
        | ast::Pattern::MatchSingleton(_)
        | ast::Pattern::MatchClass(_)
        | ast::Pattern::MatchMapping(_) => Box::new(std::iter::empty()),
    }
}

/// basedpython: visit the bare names of `pattern` that are matched against the
/// subject itself, each with whether it is one alternative of an `or` pattern.
///
/// Mirrors the walk the semantic index makes when it decides which names to
/// offer to context-sensitive resolution.
fn for_each_subject_level_case_name<'ast>(
    pattern: &'ast ast::Pattern,
    alternative: bool,
    visit: &mut impl FnMut(bool, &'ast ast::Identifier),
) {
    match pattern {
        ast::Pattern::MatchAs(ast::PatternMatchAs {
            pattern: None,
            name: Some(name),
            ..
        }) => visit(alternative, name),
        ast::Pattern::MatchAs(ast::PatternMatchAs {
            pattern: Some(inner),
            ..
        }) => for_each_subject_level_case_name(inner, alternative, visit),
        ast::Pattern::MatchOr(ast::PatternMatchOr { patterns, .. }) => {
            for pattern in patterns {
                for_each_subject_level_case_name(pattern, true, visit);
            }
        }
        ast::Pattern::MatchAnd(ast::PatternMatchAnd { patterns, .. }) => {
            for pattern in patterns {
                for_each_subject_level_case_name(pattern, alternative, visit);
            }
        }
        _ => {}
    }
}
