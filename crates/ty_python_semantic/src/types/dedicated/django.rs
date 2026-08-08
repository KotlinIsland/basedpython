//! dedicated django support — model detection and the field/member
//! machinery the `django.db.models` runtime magic needs
//!
//! types come from the `django-stubs` package. the stubs are designed
//! around their mypy plugin; everything here re-derives what that plugin
//! computes, from information the stubs themselves declare (the
//! `_pyi_private_set_type`/`_pyi_private_get_type` markers on field
//! classes, the `to=` argument of relation fields), so detection stays
//! semantic and nothing is guessed. see
//! `docs/basedpython/frameworks/django.md`

use std::borrow::Cow;

use ruff_db::parsed::ParsedModuleRef;
use ruff_python_ast as ast;
use ruff_python_ast::name::Name;
use ruff_text_size::TextRange;
use ty_module_resolver::{KnownModule, file_to_module};
use ty_python_core::definition::{Definition, DefinitionKind, DefinitionState};
use ty_python_core::{global_scope, place_table, use_def_map};

use crate::place::{Place, known_module_symbol};
use crate::types::class::{CodeGeneratorKind, Field, FieldKind};
use crate::types::member::class_member;
use crate::types::signatures::{Parameter, Parameters, Signature};
use crate::types::{
    ClassBase, ClassLiteral, ClassType, KnownClass, StaticClassLiteral, Type, UnionType,
    binding_type, definition_expression_type,
};
use crate::{Db, FxIndexMap};

pub(in crate::types) fn is_model(db: &dyn Db, class: StaticClassLiteral<'_>) -> bool {
    has_base(db, class, KnownClass::DjangoModel)
}

/// `class` is a `django.db.models.Field` subclass (relation fields included)
#[salsa::tracked(returns(copy), heap_size=ruff_memory_usage::heap_size)]
pub(in crate::types) fn is_field_class<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> bool {
    has_base(db, class, KnownClass::DjangoField)
}

/// `class` is a to-one relation field (`ForeignKey`, `OneToOneField`)
#[salsa::tracked(returns(copy), heap_size=ruff_memory_usage::heap_size)]
pub(in crate::types) fn is_relation_field_class<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> bool {
    has_base(db, class, KnownClass::DjangoForeignKey)
}

fn has_base(db: &dyn Db, class: StaticClassLiteral<'_>, base: KnownClass) -> bool {
    class
        .iter_mro(db, None)
        .filter_map(ClassBase::into_class)
        .any(|candidate| candidate.is_known(db, base))
}

/// `class` has a base declared as `name` in the module `module`
///
/// the classes this identifies (drf's view and serializer bases, django's
/// `ModelForm`) carry no runtime magic of their own, so they earn no
/// `KnownClass` variant; matching the declaring module and name keeps the
/// recognition just as exact
fn has_base_named(db: &dyn Db, class: StaticClassLiteral<'_>, module: &str, name: &str) -> bool {
    class
        .iter_mro(db, None)
        .filter_map(ClassBase::into_class)
        .filter_map(|candidate| candidate.class_literal(db).as_static())
        .any(|candidate| {
            candidate.name(db) == name
                && file_to_module(db, candidate.file(db))
                    .is_some_and(|candidate_module| candidate_module.name(db) == module)
        })
}

/// `class` is a drf view or viewset — the classes whose `queryset` names the
/// model their filter-backend field lists are resolved against
fn is_drf_generic_view(db: &dyn Db, class: StaticClassLiteral<'_>) -> bool {
    has_base_named(db, class, "rest_framework.generics", "GenericAPIView")
}

/// the model a drf view's filter-backend field lists resolve against: the
/// model its own `queryset` is a queryset of. `None` for anything else —
/// a view that builds its queryset in `get_queryset`, or inherits it, leaves
/// nothing to resolve against, and drf ignores the `model` attribute entirely
/// (it was removed in drf 3.0), so that is not a source either
#[salsa::tracked(returns(copy), heap_size=ruff_memory_usage::heap_size)]
pub(in crate::types) fn drf_view_model<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Option<StaticClassLiteral<'db>> {
    if !is_drf_generic_view(db, class) {
        return None;
    }
    // read the sibling `queryset` binding straight out of the class body:
    // resolving it as a member would infer the very scope this runs from
    let body_scope = class.body_scope(db);
    let symbol = place_table(db, body_scope).symbol_id("queryset")?;
    let use_def = use_def_map(db, body_scope);
    let mut model = None;
    for binding in use_def.end_of_scope_symbol_bindings(symbol) {
        let DefinitionState::Defined(definition) = binding.binding else {
            continue;
        };
        let candidate = queryset_or_manager_model(db, binding_type(db, definition))?;
        // more than one binding only agrees if they name the same model
        if model.is_some_and(|model| model != candidate) {
            return None;
        }
        model = Some(candidate);
    }
    model
}

/// the specialized instance type constructed by a django field constructor
/// call, or `None` when the call is not one or its facts don't resolve
///
/// the stubs declare field classes as `Field[_ST, _GT]` but leave the
/// specialization to their mypy plugin: `_ST`/`_GT` appear in no constructor
/// parameter that could solve them. instead, every concrete field class
/// declares its set/get types as `_pyi_private_set_type` /
/// `_pyi_private_get_type` markers, and relation fields use `Any` in the
/// markers where the plugin substitutes the `to=` model. this re-derives
/// exactly that: marker types, `to=` substitution, `null=True` unioning
/// `None`. anything dynamic (a string `to=`, a non-literal `null=`, a
/// custom field without markers) degrades to no pinning
pub(in crate::types) fn field_constructor_instance_type<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    to_arg: Option<Type<'db>>,
    null_arg: Option<Type<'db>>,
    through_arg: Option<Type<'db>>,
) -> Option<Type<'db>> {
    if !is_field_class(db, class) {
        return None;
    }
    let generic_context = class.generic_context(db)?;
    if generic_context.len(db) != 2 {
        return None;
    }

    // a fact that doesn't resolve degrades to no pinning, and that degradation is pinned to
    // `Unknown` explicitly: `_ST`/`_GT` appear in no constructor parameter, so leaving them to
    // the call's own inference would solve them to `Never` rather than leave them gradual
    Some(
        pinned_field_instance_type(db, class, to_arg, null_arg, through_arg)
            .unwrap_or_else(|| specialized_instance(db, class, [Type::unknown(), Type::unknown()])),
    )
}

fn pinned_field_instance_type<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    to_arg: Option<Type<'db>>,
    null_arg: Option<Type<'db>>,
    through_arg: Option<Type<'db>>,
) -> Option<Type<'db>> {
    // `ManyToManyField` is generic over `(_To, _Through)`, not `(_ST, _GT)`
    if has_base(db, class, KnownClass::DjangoManyToManyField) {
        let target = model_target_instance(db, to_arg?)?;
        let through = through_arg
            .and_then(|through| model_target_instance(db, through))
            .unwrap_or_else(Type::unknown);
        return Some(specialized_instance(db, class, [target, through]));
    }

    let set_marker = marker_type(db, class, "_pyi_private_set_type")?;
    let get_marker = marker_type(db, class, "_pyi_private_get_type")?;

    let (mut set_ty, mut get_ty) = if is_relation_field_class(db, class) {
        let target = model_target_instance(db, to_arg?)?;
        (
            replace_dynamic(db, set_marker, target),
            replace_dynamic(db, get_marker, target),
        )
    } else {
        if contains_dynamic(db, set_marker) || contains_dynamic(db, get_marker) {
            return None;
        }
        (set_marker, get_marker)
    };

    if is_null(null_arg)? {
        set_ty = UnionType::from_two_elements(db, set_ty, Type::none(db));
        get_ty = UnionType::from_two_elements(db, get_ty, Type::none(db));
    }

    Some(specialized_instance(db, class, [set_ty, get_ty]))
}

/// resolve a literal `null=` argument: absent or `False` → `Some(false)`,
/// `True` → `Some(true)`, anything dynamic → `None` (degrade to no pinning)
fn is_null(null_arg: Option<Type<'_>>) -> Option<bool> {
    match null_arg {
        None => Some(false),
        Some(ty) if ty == Type::bool_literal(true) => Some(true),
        Some(ty) if ty == Type::bool_literal(false) => Some(false),
        Some(_) => None,
    }
}

/// the instance type of a `to=`/`through=` argument, when it statically
/// resolves to a django model class
fn model_target_instance<'db>(db: &'db dyn Db, to_arg: Type<'db>) -> Option<Type<'db>> {
    let class = to_arg.as_class_literal()?;
    if !class.as_static().is_some_and(|class| is_model(db, class)) {
        return None;
    }
    to_arg.to_instance_approximation(db)
}

/// the first `name` declaration found on the mro, in mro order
fn marker_type<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<Type<'db>> {
    class
        .iter_mro(db, None)
        .filter_map(ClassBase::into_class)
        .filter_map(|base| base.static_class_literal(db))
        .find_map(|(base, _)| {
            class_member(db, base.body_scope(db), name).ignore_possibly_undefined()
        })
}

/// substitute the dynamic parts of a relation-field marker (`Any` in
/// `Any | Combinable`) with the resolved `to=` model instance type
fn replace_dynamic<'db>(db: &'db dyn Db, marker: Type<'db>, replacement: Type<'db>) -> Type<'db> {
    match marker {
        Type::Union(union) => union.map(db, |element| {
            if element.is_dynamic() {
                replacement
            } else {
                *element
            }
        }),
        ty if ty.is_dynamic() => replacement,
        ty => ty,
    }
}

fn contains_dynamic(db: &dyn Db, ty: Type<'_>) -> bool {
    match ty {
        Type::Union(union) => union.elements(db).iter().any(Type::is_dynamic),
        ty => ty.is_dynamic(),
    }
}

fn specialized_instance<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    types: [Type<'db>; 2],
) -> Type<'db> {
    let class_type = class.apply_specialization(db, |generic_context| {
        generic_context.specialize(db, Cow::Owned(types.to_vec()))
    });
    Type::instance(db, class_type)
}

/// `ty` is an instance of a `django.db.models.Field` subclass
pub(in crate::types) fn is_field_instance(db: &dyn Db, ty: Type<'_>) -> bool {
    instance_static_class(db, ty).is_some_and(|class| is_field_class(db, class))
}

fn is_relation_field_instance(db: &dyn Db, ty: Type<'_>) -> bool {
    instance_static_class(db, ty).is_some_and(|class| is_relation_field_class(db, class))
}

pub(in crate::types) fn is_many_to_many_instance(db: &dyn Db, ty: Type<'_>) -> bool {
    instance_static_class(db, ty)
        .is_some_and(|class| has_base(db, class, KnownClass::DjangoManyToManyField))
}

fn instance_static_class<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<StaticClassLiteral<'db>> {
    ty.nominal_class(db)?.class_literal(db).as_static()
}

/// the per-field facts read from a field constructor call in a class-body
/// assignment. literal arguments only; anything dynamic leaves the default
pub(in crate::types) struct FieldFacts {
    pub(in crate::types) primary_key: bool,
    pub(in crate::types) null: bool,
    pub(in crate::types) related_name: Option<Box<str>>,
    pub(in crate::types) has_choices: bool,
}

pub(in crate::types) fn field_facts<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    module: &ParsedModuleRef,
) -> FieldFacts {
    let mut facts = FieldFacts {
        primary_key: false,
        null: false,
        related_name: None,
        has_choices: false,
    };
    let DefinitionKind::Assignment(assignment) = definition.kind(db) else {
        return facts;
    };
    let Some(call) = assignment.value(module).as_call_expr() else {
        return facts;
    };
    let literal_true = |name: &str| {
        call.arguments.find_keyword(name).is_some_and(|keyword| {
            definition_expression_type(db, definition, &keyword.value) == Type::bool_literal(true)
        })
    };
    facts.primary_key = literal_true("primary_key");
    facts.null = literal_true("null");
    // django adds `get_<field>_display` for any field constructed with a
    // `choices=` argument, regardless of the argument's value
    facts.has_choices = call.arguments.find_keyword("choices").is_some();
    facts.related_name = call
        .arguments
        .find_keyword("related_name")
        .and_then(|keyword| {
            definition_expression_type(db, definition, &keyword.value).as_string_literal()
        })
        .map(|literal| Box::from(literal.value(db)));
    facts
}

/// `class` declares `Meta.abstract = True` in its own body
#[salsa::tracked(returns(copy), heap_size=ruff_memory_usage::heap_size)]
pub(in crate::types) fn is_abstract_model<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> bool {
    let Some(meta) = class_member(db, class.body_scope(db), "Meta")
        .ignore_possibly_undefined()
        .and_then(Type::as_class_literal)
        .and_then(ClassLiteral::as_static)
    else {
        return false;
    };
    class_member(db, meta.body_scope(db), "abstract")
        .ignore_possibly_undefined()
        .is_some_and(|ty| ty == Type::bool_literal(true))
}

/// the `_GT` (instance read) side of a pinned field instance type
fn field_get_type<'db>(db: &'db dyn Db, field_ty: Type<'db>) -> Option<Type<'db>> {
    let ClassType::Generic(alias) = field_ty.nominal_class(db)? else {
        return None;
    };
    let [_, get_ty] = alias.specialization(db).types(db) else {
        return None;
    };
    Some(*get_ty)
}

/// the `_ST` (assignment/lookup) side of a pinned field instance type — the
/// type django accepts when writing the field or filtering on it exactly
fn field_set_type<'db>(db: &'db dyn Db, field_ty: Type<'db>) -> Option<Type<'db>> {
    let ClassType::Generic(alias) = field_ty.nominal_class(db)? else {
        return None;
    };
    let [set_ty, _] = alias.specialization(db).types(db) else {
        return None;
    };
    Some(*set_ty)
}

/// the runtime read type of a model's primary key: the explicit
/// `primary_key=True` field's read type, or `int` for the auto `id`
/// (`BigAutoField` per modern defaults)
fn model_pk_type<'db>(db: &'db dyn Db, fields: &FxIndexMap<Name, Field<'db>>) -> Type<'db> {
    for field in fields.values() {
        if matches!(
            &field.kind,
            FieldKind::Django {
                primary_key: true,
                ..
            }
        ) {
            return field_get_type(db, field.declared_ty).unwrap_or_else(Type::unknown);
        }
    }
    KnownClass::Int.to_instance(db)
}

/// the type of a to-one relation field's `<name>_id` attname: the target
/// model's primary-key type, `| None` when the field is nullable
fn attname_type<'db>(db: &'db dyn Db, field: &Field<'db>) -> Option<Type<'db>> {
    let FieldKind::Django { null, .. } = &field.kind else {
        return None;
    };
    if !is_relation_field_instance(db, field.declared_ty) {
        return None;
    }
    let target =
        field_get_type(db, field.declared_ty)?.filter_union(db, |element| !element.is_none(db));
    let target_class = instance_static_class(db, target)?;
    if !is_model(db, target_class) {
        return None;
    }
    let target_pk = model_pk_type(db, target_class.fields(db, None, CodeGeneratorKind::Django));
    Some(if *null {
        UnionType::from_two_elements(db, target_pk, Type::none(db))
    } else {
        target_pk
    })
}

/// the members django's `ModelBase` metaclass adds to a concrete model at
/// runtime: the auto `id` primary key, the `pk` alias, and one `<field>_id`
/// attname per to-one relation field. abstract models (and `Model` itself)
/// get nothing — their concrete subclasses do
pub(in crate::types) fn synthesized_model_attribute<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    fields: &FxIndexMap<Name, Field<'db>>,
    name: &str,
) -> Option<Type<'db>> {
    if class.is_known(db, KnownClass::DjangoModel) || is_abstract_model(db, class) {
        return None;
    }
    match name {
        "pk" => Some(model_pk_type(db, fields)),
        "id" => {
            let has_explicit_pk = fields.values().any(|field| {
                matches!(
                    &field.kind,
                    FieldKind::Django {
                        primary_key: true,
                        ..
                    }
                )
            });
            (!has_explicit_pk).then(|| KnownClass::Int.to_instance(db))
        }
        _ => {
            // `get_<field>_display()` for a field declared with `choices=`
            if let Some(field_name) = name
                .strip_prefix("get_")
                .and_then(|n| n.strip_suffix("_display"))
                && matches!(
                    fields.get(field_name).map(|field| &field.kind),
                    Some(FieldKind::Django {
                        has_choices: true,
                        ..
                    })
                )
            {
                return Some(display_method(db, class));
            }
            let field_name = name.strip_suffix("_id")?;
            attname_type(db, fields.get(field_name)?)
        }
    }
}

/// the `() -> str` bound method django synthesizes for a choices field's
/// `get_<field>_display`
fn display_method<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> Type<'db> {
    let self_ty = Type::instance(db, class.default_specialization(db));
    let signature = Signature::new(
        Parameters::standard([
            Parameter::positional_or_keyword(Name::new_static("self")).with_annotated_type(self_ty)
        ]),
        KnownClass::Str.to_instance(db),
    );
    Type::function_like_callable(db, signature)
}

/// same-module reverse accessors for `class`: for every model in the file
/// declaring a to-one relation field targeting `class`, the accessor name —
/// the literal `related_name`, or `<source>_set` (bare `<source>` for
/// one-to-one) — maps to `RelatedManager[Source]` (`Source` for one-to-one).
/// cross-module relations degrade to unresolved attributes: a project-wide
/// edge index was deliberately deferred for its incrementality cost (see
/// `docs/basedpython/frameworks/django.md`)
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| FxIndexMap::default(),
    heap_size=ruff_memory_usage::heap_size,
)]
pub(in crate::types) fn reverse_accessors<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> FxIndexMap<Name, Type<'db>> {
    let mut accessors = FxIndexMap::default();
    if class.is_known(db, KnownClass::DjangoModel) || is_abstract_model(db, class) {
        return accessors;
    }

    // enumerate the module's class *definitions* structurally rather than
    // resolving every global symbol: inferring arbitrary bindings from here
    // can cycle back into member lookups that consult this query
    let global = global_scope(db, class.file(db));
    let use_def = use_def_map(db, global);
    let mut sources = Vec::new();
    for (_, bindings) in use_def.all_end_of_scope_symbol_bindings() {
        for binding in bindings {
            if let DefinitionState::Defined(definition) = binding.binding
                && definition.kind(db).as_class().is_some()
                && let Type::ClassLiteral(ClassLiteral::Static(source)) =
                    binding_type(db, definition)
            {
                sources.push(source);
            }
        }
    }

    for source in sources {
        if !is_model(db, source) {
            continue;
        }
        for field in source.fields(db, None, CodeGeneratorKind::Django).values() {
            let is_m2m = is_many_to_many_instance(db, field.declared_ty);
            if !is_relation_field_instance(db, field.declared_ty) && !is_m2m {
                continue;
            }

            // the target of a to-one field is its `_GT`; for a many-to-many
            // field it is the first (`_To`) specialization argument, and the
            // second (`_Through`) carries the through model
            let (target, through) = if is_m2m {
                match m2m_target_and_through(db, field.declared_ty) {
                    Some((target, through)) => (target, Some(through)),
                    None => continue,
                }
            } else {
                match field_get_type(db, field.declared_ty) {
                    Some(target) => (
                        target.filter_union(db, |element| !element.is_none(db)),
                        None,
                    ),
                    None => continue,
                }
            };
            if instance_static_class(db, target) != Some(class) {
                continue;
            }
            let FieldKind::Django { related_name, .. } = &field.kind else {
                continue;
            };
            // `related_name="+"` disables the reverse accessor
            if related_name.as_deref() == Some("+") {
                continue;
            }
            let one_to_one =
                instance_static_class(db, field.declared_ty).is_some_and(|field_class| {
                    has_base(db, field_class, KnownClass::DjangoOneToOneField)
                });
            let accessor = match related_name {
                Some(name) => Name::new(&**name),
                None if one_to_one => Name::new(source.name(db).to_lowercase()),
                None => Name::new(format!("{}_set", source.name(db).to_lowercase())),
            };
            let source_instance = Type::instance(db, source.default_specialization(db));
            let accessor_ty = if let Some(through) = through {
                // the reverse of a many-to-many is itself a many-to-many manager
                many_related_manager_instance(db, source_instance, through)
            } else if one_to_one {
                Some(source_instance)
            } else {
                related_manager_instance(db, source_instance)
            };
            if let Some(accessor_ty) = accessor_ty {
                accessors.insert(accessor, accessor_ty);
            }
        }
    }
    accessors.shrink_to_fit();
    accessors
}

/// the `(_To, _Through)` specialization of a `ManyToManyField` instance — its
/// target model and through model (the latter `Unknown` for an implicit table)
fn m2m_target_and_through<'db>(
    db: &'db dyn Db,
    field_ty: Type<'db>,
) -> Option<(Type<'db>, Type<'db>)> {
    let ClassType::Generic(alias) = field_ty.nominal_class(db)? else {
        return None;
    };
    let [target, through] = alias.specialization(db).types(db) else {
        return None;
    };
    Some((*target, *through))
}

/// `ManyRelatedManager[source, through]` from the stubs' `related_descriptors`
/// module, or `None` when it doesn't resolve (degrade to no accessor)
fn many_related_manager_instance<'db>(
    db: &'db dyn Db,
    source: Type<'db>,
    through: Type<'db>,
) -> Option<Type<'db>> {
    let manager = known_module_symbol(
        db,
        KnownModule::DjangoDbModelsFieldsRelatedDescriptors,
        "ManyRelatedManager",
    )
    .place
    .ignore_possibly_undefined()?;
    let class = manager.as_class_literal()?.as_static()?;
    let generic_context = class.generic_context(db)?;
    let args = match generic_context.len(db) {
        1 => vec![source],
        2 => vec![source, through],
        _ => return None,
    };
    let class_type =
        class.apply_specialization(db, |generic_context| generic_context.specialize(db, args));
    Some(Type::instance(db, class_type))
}

/// `RelatedManager[source]` from the stubs' `related_descriptors` module,
/// or `None` when it doesn't resolve (degrade to no accessor)
fn related_manager_instance<'db>(db: &'db dyn Db, source: Type<'db>) -> Option<Type<'db>> {
    let manager = known_module_symbol(
        db,
        KnownModule::DjangoDbModelsFieldsRelatedDescriptors,
        "RelatedManager",
    )
    .place
    .ignore_possibly_undefined()?;
    let class = manager.as_class_literal()?.as_static()?;
    let generic_context = class.generic_context(db)?;
    if generic_context.len(db) != 1 {
        return None;
    }
    let class_type = class.apply_specialization(db, |generic_context| {
        generic_context.specialize(db, Cow::Owned(vec![source]))
    });
    Some(Type::instance(db, class_type))
}

/// the member names `synthesized_model_attribute` can synthesize for
/// `class`, for member listing (IDE completions)
pub(in crate::types) fn synthesized_member_names<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    fields: &FxIndexMap<Name, Field<'db>>,
) -> Vec<Name> {
    if class.is_known(db, KnownClass::DjangoModel) || is_abstract_model(db, class) {
        return Vec::new();
    }
    let mut names = vec![Name::new_static("pk")];
    let has_explicit_pk = fields.values().any(|field| {
        matches!(
            &field.kind,
            FieldKind::Django {
                primary_key: true,
                ..
            }
        )
    });
    if !has_explicit_pk {
        names.push(Name::new_static("id"));
    }
    for (name, field) in fields {
        if is_relation_field_instance(db, field.declared_ty) {
            names.push(Name::new(format!("{name}_id")));
        }
    }
    names.extend(reverse_accessors(db, class).keys().cloned());
    names
}

/// extra keyword parameters django's `Model.__init__` accepts beyond the
/// field names themselves: the `pk` alias and the `<field>_id` attnames.
/// all optional and none-able — requiredness is a `save` concern
pub(in crate::types) fn extra_constructor_parameters<'db>(
    db: &'db dyn Db,
    fields: &FxIndexMap<Name, Field<'db>>,
) -> Vec<(Name, Type<'db>)> {
    let mut extras = Vec::new();
    if !fields.contains_key("pk") {
        extras.push((
            Name::new_static("pk"),
            UnionType::from_two_elements(db, model_pk_type(db, fields), Type::none(db)),
        ));
    }
    for (name, field) in fields {
        let Some(attname_ty) = attname_type(db, field) else {
            continue;
        };
        let attname = Name::new(format!("{name}_id"));
        if fields.contains_key(&attname) {
            continue;
        }
        extras.push((
            attname,
            UnionType::from_two_elements(db, attname_ty, Type::none(db)),
        ));
    }
    extras
}

// ---------------------------------------------------------------------------
// queryset / manager call validation
//
// django's `filter`/`get`/`create`/… accept `**kwargs: Any` in the stubs; the
// mypy plugin validates them against the model's fields at the call site. we
// re-derive the same checks statically from the field list. everything here is
// deliberately conservative — an unrecognized lookup, a relation to a model we
// can't resolve, or a non-literal key all degrade to "no problem" rather than
// risk a false positive on valid code (custom lookups, lazy references)
// ---------------------------------------------------------------------------

/// the model a `Manager[M]` / `QuerySet[M, _]` instance is parameterized by
pub(in crate::types) fn queryset_or_manager_model<'db>(
    db: &'db dyn Db,
    self_instance: Type<'db>,
) -> Option<StaticClassLiteral<'db>> {
    let class = self_instance.nominal_class(db)?;
    let is_qs_or_manager = class.class_literal(db).as_static().is_some_and(|literal| {
        has_base(db, literal, KnownClass::DjangoManager)
            || has_base(db, literal, KnownClass::DjangoQuerySet)
    });
    if !is_qs_or_manager {
        return None;
    }
    let ClassType::Generic(alias) = class else {
        return None;
    };
    let model_instance = alias.specialization(db).types(db).first()?;
    let model = instance_static_class(db, *model_instance)?;
    is_model(db, model).then_some(model)
}

/// how a `Manager`/`QuerySet` method treats its arguments, for validation
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::types) enum QuerysetMethodKind {
    /// `**kwargs` are field lookups (`name__startswith="x"`)
    Lookup,
    /// `**kwargs` are field assignments (`create(name="x")`)
    Create,
    /// positional `*args` are literal field-name strings (`order_by("name")`)
    FieldNames,
}

pub(in crate::types) fn queryset_method_kind(name: &str) -> Option<QuerysetMethodKind> {
    match name {
        "filter" | "get" | "exclude" | "get_or_create" | "update_or_create" | "aget"
        | "aget_or_create" | "aupdate_or_create" => Some(QuerysetMethodKind::Lookup),
        "create" | "acreate" => Some(QuerysetMethodKind::Create),
        "order_by" | "only" | "defer" | "values" | "values_list" | "earliest" | "latest"
        | "aearliest" | "alatest" => Some(QuerysetMethodKind::FieldNames),
        _ => None,
    }
}

/// whether `key` is a keyword the method `method` declares itself, rather than
/// a field lookup. the `*_or_create` family carries the values to apply on
/// create in `defaults` (and, since django 5.0, `create_defaults` on
/// `update_or_create`), so those names are django's own
pub(in crate::types) fn is_method_own_keyword(method: &str, key: &str) -> bool {
    match method {
        "get_or_create" | "aget_or_create" => key == "defaults",
        "update_or_create" | "aupdate_or_create" => key == "defaults" || key == "create_defaults",
        _ => false,
    }
}

/// the outcome of resolving one lookup / field-name key against a model
pub(in crate::types) enum FieldResolution<'db> {
    /// the leading segment names no field of `model`
    Unknown { model: Box<str>, segment: Box<str> },
    /// resolved; `operand` is the value type the lookup expects, or `None`
    /// when the lookup has no statically-checkable operand (relations,
    /// iterables, chained transforms, custom lookups)
    Resolved { operand: Option<Type<'db>> },
}

/// a field reachable by name from a model, for path walking
struct FieldRef<'db> {
    relation_model: Option<StaticClassLiteral<'db>>,
    is_relation: bool,
    /// the type accepted when writing / filtering the field (`_ST`)
    set_type: Type<'db>,
    /// the type read back from a `values()`/`values_list()` row — the field's
    /// `_GT` for a concrete field, the target pk for a bare relation
    value_type: Type<'db>,
}

/// resolve `name` (a real field, `pk`, `id`, or an `<fk>_id` attname) on
/// `model` to a reference for path walking
fn field_ref<'db>(
    db: &'db dyn Db,
    model: StaticClassLiteral<'db>,
    name: &str,
) -> Option<FieldRef<'db>> {
    let fields = model.fields(db, None, CodeGeneratorKind::Django);

    if name == "pk" {
        let pk = model_pk_type(db, fields);
        return Some(FieldRef {
            relation_model: None,
            is_relation: false,
            set_type: pk,
            value_type: pk,
        });
    }

    if let Some(field) = fields.get(name) {
        if is_relation_field_instance(db, field.declared_ty) {
            let target = field_get_type(db, field.declared_ty)
                .map(|ty| ty.filter_union(db, |element| !element.is_none(db)));
            let relation_model = target
                .and_then(|target| instance_static_class(db, target))
                .filter(|target| is_model(db, *target));
            // a bare relation in a `values()` row reads as the target's pk
            let value_type = relation_model.map_or_else(Type::unknown, |target| {
                model_pk_type(db, target.fields(db, None, CodeGeneratorKind::Django))
            });
            return Some(FieldRef {
                relation_model,
                is_relation: true,
                set_type: Type::unknown(),
                value_type,
            });
        }
        return Some(FieldRef {
            relation_model: None,
            is_relation: false,
            set_type: field_set_type(db, field.declared_ty).unwrap_or_else(Type::unknown),
            value_type: field_get_type(db, field.declared_ty).unwrap_or_else(Type::unknown),
        });
    }

    // synthesized names: the auto `id` and `<fk>_id` attnames
    if let Some(synthesized) = synthesized_model_attribute(db, model, fields, name) {
        return Some(FieldRef {
            relation_model: None,
            is_relation: false,
            set_type: synthesized,
            value_type: synthesized,
        });
    }

    None
}

/// the operand type a single recognized lookup expects on a concrete field of
/// `set_type`; `None` for lookups without a checkable operand
fn concrete_lookup_operand<'db>(
    db: &'db dyn Db,
    set_type: Type<'db>,
    lookups: &[&str],
) -> Option<Type<'db>> {
    // chained transforms (`date__year`) can't be resolved statically here
    let [lookup] = lookups else {
        return None;
    };
    match *lookup {
        "exact" | "iexact" | "gt" | "gte" | "lt" | "lte" => Some(set_type),
        "contains" | "icontains" | "startswith" | "istartswith" | "endswith" | "iendswith"
        | "regex" | "iregex" | "search" | "trigram_similar" => {
            Some(KnownClass::Str.to_instance(db))
        }
        "isnull" => Some(KnownClass::Bool.to_instance(db)),
        // `in`/`range` take iterables, date/time transforms chain further —
        // skip rather than risk a false positive
        _ => None,
    }
}

/// django's built-in lookups that apply directly to a relation field
fn is_relation_lookup(name: &str) -> bool {
    matches!(
        name,
        "exact" | "iexact" | "in" | "isnull" | "gt" | "gte" | "lt" | "lte" | "range"
    )
}

/// resolve a lookup key (`author__name__startswith`) against `model`
pub(in crate::types) fn resolve_lookup<'db>(
    db: &'db dyn Db,
    model: StaticClassLiteral<'db>,
    key: &str,
) -> FieldResolution<'db> {
    let segments: Vec<&str> = key.split("__").collect();
    let mut model = model;
    let mut index = 0;

    loop {
        let segment = segments[index];
        let is_last = index + 1 == segments.len();

        let Some(field) = field_ref(db, model, segment) else {
            // only the leading segment is unambiguously a field position; a
            // later unknown segment was already classified as a lookup below
            return FieldResolution::Unknown {
                model: model.name(db).as_str().into(),
                segment: segment.into(),
            };
        };

        if field.is_relation {
            if is_last {
                // `filter(author=…)` accepts an instance or a pk — skip typing
                return FieldResolution::Resolved { operand: None };
            }
            let Some(target) = field.relation_model else {
                // lazy / unresolved relation target — can't validate further
                return FieldResolution::Resolved { operand: None };
            };
            let next = segments[index + 1];
            // after a relation hop the next segment is expected to be a field
            // on the target model — traverse into it
            if field_ref(db, target, next).is_some() {
                model = target;
                index += 1;
                continue;
            }
            // otherwise it must be a relation-level lookup (`author__isnull`);
            // a final segment that is neither a field nor a recognized lookup
            // is almost certainly a typo
            if index + 2 == segments.len() && is_relation_lookup(next) {
                let operand = match next {
                    "isnull" => Some(KnownClass::Bool.to_instance(db)),
                    _ => None,
                };
                return FieldResolution::Resolved { operand };
            }
            return FieldResolution::Unknown {
                model: target.name(db).as_str().into(),
                segment: next.into(),
            };
        }

        if is_last {
            return FieldResolution::Resolved {
                operand: Some(field.set_type),
            };
        }
        let operand = concrete_lookup_operand(db, field.set_type, &segments[index + 1..]);
        return FieldResolution::Resolved { operand };
    }
}

/// resolve a `create()` keyword: the field's assignable type, or `Unknown`
/// when the key names no field / attname of the model
pub(in crate::types) fn resolve_create_kwarg<'db>(
    db: &'db dyn Db,
    model: StaticClassLiteral<'db>,
    key: &str,
) -> FieldResolution<'db> {
    // create() keys are plain field names or attnames — no `__` traversal
    match field_ref(db, model, key) {
        Some(field) if field.is_relation => FieldResolution::Resolved { operand: None },
        Some(field) => FieldResolution::Resolved {
            operand: Some(field.set_type),
        },
        None => FieldResolution::Unknown {
            model: model.name(db).as_str().into(),
            segment: key.into(),
        },
    }
}

/// validate a positional field-name string (`order_by("name")`); a leading
/// `-` (descending) is stripped. returns the offending segment when unknown
pub(in crate::types) fn resolve_field_name<'db>(
    db: &'db dyn Db,
    model: StaticClassLiteral<'db>,
    name: &str,
) -> FieldResolution<'db> {
    // `?` (random order) and query expressions aren't field names
    let name = name.strip_prefix('-').unwrap_or(name);
    if name == "?" || name.is_empty() {
        return FieldResolution::Resolved { operand: None };
    }
    match resolve_lookup(db, model, name) {
        FieldResolution::Unknown { model, segment } => FieldResolution::Unknown { model, segment },
        FieldResolution::Resolved { .. } => FieldResolution::Resolved { operand: None },
    }
}

// ---------------------------------------------------------------------------
// class-body field-name lists
//
// several django / drf constructs declare model field paths as a class-body
// list of strings rather than as call arguments. each is validated against a
// model the declaring class names, and each has its own spelling rules. the
// model source differs per site, so the builder resolves it and calls back
// into `field_list_kind` for the per-entry spelling
// ---------------------------------------------------------------------------

/// the kind of `Meta.fields` / `Meta.exclude` declarer a class is
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::types) enum MetaFieldsDeclarer {
    /// a drf `ModelSerializer`. every entry must name something reachable on
    /// the model — a field, but equally a property, a method or a reverse
    /// accessor, since drf falls back to a read-only property field — or a
    /// field the serializer itself declares
    ModelSerializer,
    /// a django `ModelForm`. `fields` is stricter than drf (only editable
    /// concrete fields), so the drf rule under-reports here rather than
    /// risking a false positive. `exclude` is not checkable at all: django
    /// silently ignores an entry that names nothing
    ModelForm,
}

pub(in crate::types) fn meta_fields_declarer(
    db: &dyn Db,
    class: StaticClassLiteral<'_>,
) -> Option<MetaFieldsDeclarer> {
    if has_base_named(db, class, "rest_framework.serializers", "ModelSerializer") {
        Some(MetaFieldsDeclarer::ModelSerializer)
    } else if has_base_named(db, class, "django.forms.models", "ModelForm") {
        Some(MetaFieldsDeclarer::ModelForm)
    } else {
        None
    }
}

impl MetaFieldsDeclarer {
    /// whether an entry of the class-body attribute `name` is worth resolving
    pub(in crate::types) fn checks(self, name: &str) -> bool {
        match self {
            Self::ModelSerializer => name == "fields" || name == "exclude",
            // django drops an unknown `exclude` entry without complaint
            Self::ModelForm => name == "fields",
        }
    }
}

/// whether `name` is something a `Meta.fields` entry may legally name: an
/// attribute of the model (drf builds a read-only property field for a
/// non-field attribute, so a method or property is as valid as a field), or a
/// field the declaring serializer / form declares itself
///
/// the declared-field side is read out of the place tables rather than by
/// member lookup: the declaring class's body is the scope this runs from
pub(in crate::types) fn is_meta_fields_entry_valid<'db>(
    db: &'db dyn Db,
    model: StaticClassLiteral<'db>,
    declarer: StaticClassLiteral<'db>,
    name: &str,
) -> bool {
    let model_instance = Type::instance(db, model.default_specialization(db));
    if !matches!(model_instance.member(db, name).place, Place::Undefined) {
        return true;
    }
    declarer
        .iter_mro(db, None)
        .filter_map(ClassBase::into_class)
        .filter_map(|base| base.class_literal(db).as_static())
        .any(|base| {
            place_table(db, base.body_scope(db))
                .symbol_id(name)
                .is_some()
        })
}

/// the model a nested `Meta` names, read from its own `model = <Model>`
/// binding. `None` when there is none, or it doesn't resolve to a model
pub(in crate::types) fn meta_model<'db>(
    db: &'db dyn Db,
    meta: StaticClassLiteral<'db>,
) -> Option<StaticClassLiteral<'db>> {
    let body_scope = meta.body_scope(db);
    let symbol = place_table(db, body_scope).symbol_id("model")?;
    let use_def = use_def_map(db, body_scope);
    let mut model = None;
    for binding in use_def.end_of_scope_symbol_bindings(symbol) {
        let DefinitionState::Defined(definition) = binding.binding else {
            continue;
        };
        let candidate = binding_type(db, definition)
            .as_class_literal()
            .and_then(ClassLiteral::as_static)
            .filter(|candidate| is_model(db, *candidate))?;
        if model.is_some_and(|model| model != candidate) {
            return None;
        }
        model = Some(candidate);
    }
    model
}

/// a class-body attribute whose value is a list of model field paths
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::types) enum FieldListKind<'db> {
    /// a model's `Meta.ordering`: `order_by` syntax, so a leading `-` and the
    /// `?` sentinel are both legal. django reports a bad entry itself, as
    /// `models.E015`
    Ordering,
    /// a drf view's `ordering_fields` / `filterset_fields`: plain field paths.
    /// the whole attribute may instead be the `"__all__"` sentinel
    ViewFieldNames,
    /// a drf view's `search_fields`: a field path optionally carrying one of
    /// `SearchFilter`'s lookup prefixes
    SearchFieldNames,
    /// a serializer's / form's `Meta.fields` or `Meta.exclude`: a plain name,
    /// resolved against the model's attributes and `declaring`'s own fields
    MetaFields { declaring: StaticClassLiteral<'db> },
}

/// the field-list site a class-body attribute named `name` declares, for a
/// class that has already been established as the right kind of declarer
pub(in crate::types) fn view_field_list_kind<'db>(name: &str) -> Option<FieldListKind<'db>> {
    match name {
        "ordering_fields" | "filterset_fields" => Some(FieldListKind::ViewFieldNames),
        "search_fields" => Some(FieldListKind::SearchFieldNames),
        _ => None,
    }
}

/// the literal string entries of a class-body field-name list, with their
/// ranges. `None` when the value is not a list this can read exhaustively —
/// a comprehension, a name, a concatenation, or a list holding any
/// non-literal element — because a partially-read list can't distinguish a
/// typo from an entry supplied elsewhere
pub(in crate::types) fn literal_field_list_entries(
    value: &ast::Expr,
) -> Option<Vec<(&str, TextRange)>> {
    let elements = match value {
        // `fields = "__all__"` and drf's `ordering_fields = "__all__"`: a bare
        // string is a sentinel, never a field name
        ast::Expr::StringLiteral(_) => return Some(Vec::new()),
        ast::Expr::List(list) => &list.elts,
        ast::Expr::Tuple(tuple) => &tuple.elts,
        // `filterset_fields = {"title": ["exact"]}` maps a field path to the
        // lookups allowed on it — the keys are the field paths
        ast::Expr::Dict(dict) => {
            return dict
                .items
                .iter()
                .map(|item| {
                    let literal = item.key.as_ref()?.as_string_literal_expr()?;
                    Some((literal.value.to_str(), literal.range))
                })
                .collect();
        }
        _ => return None,
    };
    elements
        .iter()
        .map(|element| {
            let literal = element.as_string_literal_expr()?;
            Some((literal.value.to_str(), literal.range))
        })
        .collect()
}

impl<'db> FieldListKind<'db> {
    /// resolve one literal entry of the list against `model`
    pub(in crate::types) fn resolve(
        self,
        db: &'db dyn Db,
        model: StaticClassLiteral<'db>,
        entry: &str,
    ) -> FieldResolution<'db> {
        match self {
            // `order_by` syntax — `-` and `?` included
            Self::Ordering => resolve_field_name(db, model, entry),
            Self::ViewFieldNames => resolve_lookup(db, model, entry),
            Self::MetaFields { declaring } => {
                if is_meta_fields_entry_valid(db, model, declaring, entry) {
                    FieldResolution::Resolved { operand: None }
                } else {
                    FieldResolution::Unknown {
                        model: model.name(db).as_str().into(),
                        segment: entry.into(),
                    }
                }
            }
            Self::SearchFieldNames => {
                // `SearchFilter` reads a leading `^` (istartswith), `=`
                // (iexact), `@` (full-text search) or `$` (iregex) off the
                // entry before using the rest as a field path
                let path = entry.strip_prefix(['^', '=', '@', '$']).unwrap_or(entry);
                if path.is_empty() {
                    return FieldResolution::Resolved { operand: None };
                }
                resolve_lookup(db, model, path)
            }
        }
    }
}

/// the read type of a `field__path` for a `values()`/`values_list()` row: the
/// terminal field's `_GT` (a bare relation reads as the target pk). `None`
/// when the path can't be statically resolved (so refinement is skipped)
fn field_value_type<'db>(
    db: &'db dyn Db,
    model: StaticClassLiteral<'db>,
    key: &str,
) -> Option<Type<'db>> {
    let segments: Vec<&str> = key.split("__").collect();
    let mut model = model;
    for (index, segment) in segments.iter().enumerate() {
        let field = field_ref(db, model, segment)?;
        let is_last = index + 1 == segments.len();
        if field.is_relation && !is_last {
            // traverse into the related model for the next segment
            model = field.relation_model?;
            continue;
        }
        // a concrete terminal, or a bare relation terminal (reads as pk)
        return is_last.then_some(field.value_type);
    }
    None
}

/// refine a `values_list(*fields, flat=, named=)` call's row type: the tuple of
/// the fields' read types, the single read type when `flat=True`, `None` to
/// keep the stub type (`named=True`, no fields, or any unresolved field)
pub(in crate::types) fn values_list_row_type<'db>(
    db: &'db dyn Db,
    model: StaticClassLiteral<'db>,
    fields: &[&str],
    flat: bool,
    named: bool,
) -> Option<Type<'db>> {
    // a synthesized namedtuple is out of scope; `values_list()` with no fields
    // selects every field — neither is refined here
    if named || fields.is_empty() {
        return None;
    }
    let values: Option<Vec<Type<'db>>> = fields
        .iter()
        .map(|field| field_value_type(db, model, field))
        .collect();
    let values = values?;
    if flat {
        // `flat=True` is only valid with a single field
        return (values.len() == 1).then(|| values[0]);
    }
    Some(Type::heterogeneous_tuple(db, values))
}

/// refine a `values(*fields)` call's row type to `dict[str, <union of the
/// fields' read types>]` (monotonically more precise than the stub's
/// `dict[str, Any]`). `None` to keep the stub type
pub(in crate::types) fn values_row_type<'db>(
    db: &'db dyn Db,
    model: StaticClassLiteral<'db>,
    fields: &[&str],
) -> Option<Type<'db>> {
    if fields.is_empty() {
        return None;
    }
    let values: Option<Vec<Type<'db>>> = fields
        .iter()
        .map(|field| field_value_type(db, model, field))
        .collect();
    let value = UnionType::from_elements(db, values?);
    Some(KnownClass::Dict.to_specialized_instance(db, &[KnownClass::Str.to_instance(db), value]))
}

/// rebuild a `QuerySet[Model, _Row]` return type with a refined `_Row`
pub(in crate::types) fn with_queryset_row<'db>(
    db: &'db dyn Db,
    queryset_ty: Type<'db>,
    row: Type<'db>,
) -> Option<Type<'db>> {
    let ClassType::Generic(alias) = queryset_ty.nominal_class(db)? else {
        return None;
    };
    let [model_arg, _] = alias.specialization(db).types(db) else {
        return None;
    };
    let model_arg = *model_arg;
    let class_type = alias
        .origin(db)
        .apply_specialization(db, |generic_context| {
            generic_context.specialize(db, Cow::Owned(vec![model_arg, row]))
        });
    Some(Type::instance(db, class_type))
}
