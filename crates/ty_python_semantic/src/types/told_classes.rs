//! basedpython: what the runtime is told about the classes a stub describes
//!
//! a type test the checker leaves to the runtime asks whether one type argument is a subtype of
//! another, and what the checker reads of a class of the standard library is in no object the
//! runtime holds: `class str(Sequence[str])` is a `str` whose `__bases__` is `(object,)`, and
//! `class Iterable(Protocol[T])` is an abc that says nothing of being a protocol. a class whose
//! definition runs says all of that for itself, by its bases and its annotations
//!
//! what is told is data, as `_by_told` in the runtime describes, each name read as what it means
//! in the module the stub describes. the program's rows are in the one file that holds its
//! helpers, and a module with its helpers pasted in holds them only if it tests something

use std::collections::BTreeMap;
use std::sync::Arc;

use ruff_python_ast::PySourceType;
use rustc_hash::FxHashSet;

use crate::Db;
use crate::ProgramEnvironment;
use crate::dunder_all::dunder_all_names;
use crate::place::{Place, global_symbol};
use crate::types::MemberLookupPolicy;
use crate::types::class::{ClassLiteral, ClassType, StaticClassLiteral};
use crate::types::class_base::ClassBase;
use crate::types::literal::LiteralValueTypeKind;
use crate::types::reified_infer::{
    ArgVariance, class_variances, python_bytes_literal, python_str_literal,
};
use crate::types::signatures::Signature;
use crate::types::special_form::SpecialFormType;
use crate::types::tuple::TupleSpec;
use crate::types::typevar::BoundTypeVarInstance;
use crate::types::{KnownClass, Type};
use ty_module_resolver::{KnownModule, file_to_module, resolve_module_confident};

/// what the runtime is told, a row of python data by the path that names what it describes
pub(crate) type ToldClasses = Arc<BTreeMap<String, String>>;

/// everything the runtime has to be told to read `ty`, an instance type a module reads, in which a
/// class literal is what a final class's `type[C]` reads as
pub(crate) fn told_classes_of_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> ToldClasses {
    let mut reading = Reading::new(db, env);
    reading.part(ty);
    reading.told()
}

/// everything the runtime has to be told to read `ty`, what a name that reads a class or a
/// subscript that specializes one reads as, a base of a class included
pub(crate) fn told_classes_of_class_read<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> ToldClasses {
    let mut reading = Reading::new(db, env);
    match ty {
        Type::ClassLiteral(class) => reading.class(class),
        _ => reading.part(ty),
    }
    reading.told()
}

/// everything the runtime has to be told to read `class`
pub(crate) fn told_classes_of_class<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> ToldClasses {
    match class {
        ClassLiteral::Static(class) => rows_of_class(db, class).clone(),
        _ => ToldClasses::default(),
    }
}

/// the classes one read of a type reaches
struct Reading<'a, 'db> {
    db: &'db dyn Db,
    env: &'a ProgramEnvironment<'db>,
    told: BTreeMap<String, String>,
    parts: FxHashSet<Type<'db>>,
    /// the aliases already opened, by what defines them. one is opened once whatever it is
    /// specialized with: its value names the same classes each time, what it is specialized with
    /// is read at every use, and `type G[T] = list[G[list[T]]]` never comes back to the same type
    aliases: FxHashSet<ty_python_core::definition::Definition<'db>>,
}

impl<'a, 'db> Reading<'a, 'db> {
    fn new(db: &'db dyn Db, env: &'a ProgramEnvironment<'db>) -> Self {
        Self {
            db,
            env,
            told: BTreeMap::new(),
            parts: FxHashSet::default(),
            aliases: FxHashSet::default(),
        }
    }

    fn told(self) -> ToldClasses {
        Arc::new(self.told)
    }

    fn class(&mut self, class: ClassLiteral<'db>) {
        if let ClassLiteral::Static(class) = class {
            self.told.extend(
                rows_of_class(self.db, class)
                    .iter()
                    .map(|(path, row)| (path.clone(), row.clone())),
            );
        }
    }

    fn part(&mut self, part: Type<'db>) {
        if !self.parts.insert(part) {
            return;
        }
        let (db, env) = (self.db, self.env);
        match part {
            Type::NominalInstance(instance) => self.class_type(instance.class(db, env)),
            Type::ProtocolInstance(protocol) => {
                if let Some(origin) = protocol.class_origin(db) {
                    self.class_type(*origin);
                }
            }
            Type::ClassLiteral(class) => {
                self.called(class);
                self.class(class);
            }
            Type::GenericAlias(alias) => self.class_type(ClassType::Generic(alias)),
            Type::SubclassOf(subclass_of) => {
                if let Some(class) = subclass_of.subclass_of().into_class(db, env) {
                    self.called(class.class_literal(db));
                    self.class_type(class);
                }
            }
            Type::LiteralValue(literal) => self.part(literal.fallback_instance(db, env)),
            Type::TypedDict(typed_dict) => {
                if let Some(class) = typed_dict.defining_class() {
                    self.class_type(class);
                }
            }
            Type::Union(union) => {
                for element in union.elements(db) {
                    self.part(*element);
                }
            }
            Type::Intersection(intersection) => {
                for element in intersection.positive(db) {
                    self.part(*element);
                }
            }
            Type::Callable(callable) => {
                for signature in callable.signatures(db) {
                    for parameter in signature.parameters() {
                        self.part(parameter.annotated_type());
                    }
                    self.part(signature.return_ty);
                }
            }
            Type::TypeAlias(alias) => {
                if let Some(specialization) = alias.specialization(db) {
                    for argument in specialization.types(db) {
                        self.part(*argument);
                    }
                }
                if self.aliases.insert(alias.definition(db)) {
                    self.part(alias.value_type(db));
                }
            }
            Type::NewTypeInstance(newtype) => self.part(newtype.concrete_base_type(db)),
            _ => {}
        }
    }

    fn class_type(&mut self, class: ClassType<'db>) {
        let db = self.db;
        self.class(class.class_literal(db));
        let ClassType::Generic(alias) = class else {
            return;
        };
        let specialization = alias.specialization(db);
        if let Some(spec) = specialization.tuple(db) {
            for element in spec.iter_element_types(db) {
                self.part(element);
            }
        } else {
            for argument in specialization.types(db) {
                self.part(*argument);
            }
        }
    }

    /// a class read as `type[C]`, whose constructor a test can ask about. one a stub describes
    /// has no annotations to read it from
    fn called(&mut self, class: ClassLiteral<'db>) {
        if let ClassLiteral::Static(class) = class
            && let Some((path, row)) = constructors_of_class(self.db, class)
        {
            self.told.insert(path.clone(), row.clone());
        }
    }
}

/// the rows of `class` and of every class of a stub its own rows read
#[salsa::tracked]
fn rows_of_class<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> ToldClasses {
    let env = ProgramEnvironment::from_file(db.program_file(class.file(db)));
    let mut writing = Writing {
        db,
        env: &env,
        seen: FxHashSet::default(),
        pending: vec![ClassLiteral::Static(class)],
    };
    let mut rows = BTreeMap::new();
    while let Some(class) = writing.pending.pop() {
        if !writing.seen.insert(class) {
            continue;
        }
        for base in class.iter_mro(db) {
            if let ClassBase::Class(base) = base {
                writing.pending.push(base.class_literal(db));
            }
        }
        // the checker reads a `TypedDict` as a `Mapping[str, object]`
        if class.is_typed_dict(db)
            && let Some(mapping) = KnownClass::Mapping
                .to_class_literal(db, &env)
                .as_class_literal()
        {
            writing.pending.push(mapping);
        }
        if let Some(row) = writing.row(class) {
            rows.insert(class_path(db, class), row);
        }
    }
    Arc::new(rows)
}

/// the row saying how the class of a stub is called, if it is one
#[salsa::tracked(returns(ref))]
fn constructors_of_class<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Option<(String, String)> {
    if !class.file(db).source_type(db).is_stub() {
        return None;
    }
    let env = ProgramEnvironment::from_file(db.program_file(class.file(db)));
    let literal = ClassLiteral::Static(class);
    let mut writing = Writing {
        db,
        env: &env,
        seen: FxHashSet::default(),
        pending: Vec::new(),
    };
    let params = class_params(db, literal);
    let callables = Type::ClassLiteral(literal).try_upcast_to_callable(db, &env)?;
    let mut ways = Vec::new();
    for callable in &callables {
        for signature in callable.signatures(db) {
            ways.extend(writing.way(signature, &params, false));
        }
    }
    let path = format!("{}()", class_path(db, literal));
    let row = tuple([python_str_literal(&path), tuple(ways.into_iter())].into_iter());
    Some((path, row))
}

/// the names of the members of a protocol `typing` or `collections.abc` declares, which is what a
/// test can name: a stub says of each class what each of those is
#[salsa::tracked(returns(ref))]
fn member_names_a_test_can_ask_about(db: &dyn Db, file: ruff_db::files::File) -> Vec<String> {
    let file = db.program_file(file);
    let env = ProgramEnvironment::from_file(file);
    let mut names = std::collections::BTreeSet::new();
    for module in [KnownModule::Typing, KnownModule::CollectionsAbcInternal] {
        let Some(found) =
            resolve_module_confident(db, env.resolver_environment(db), &module.name())
                .and_then(|module| module.file(db))
        else {
            continue;
        };
        let found = db.program_file(found);
        for exported in dunder_all_names(db, found).into_iter().flatten() {
            let Place::Defined(defined) = global_symbol(db, found, exported.as_str()).place else {
                continue;
            };
            if let Some(protocol) = defined
                .ty
                .as_class_literal()
                .and_then(|class| class.into_protocol_class(db))
            {
                names.extend(
                    protocol
                        .interface(db)
                        .members(db)
                        .map(|member| member.name().to_owned()),
                );
            }
        }
    }
    names.into_iter().collect()
}

fn class_params<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> Vec<BoundTypeVarInstance<'db>> {
    class
        .generic_context(db)
        .map(|context| context.variables(db).collect())
        .unwrap_or_default()
}

struct Writing<'a, 'db> {
    db: &'db dyn Db,
    env: &'a ProgramEnvironment<'db>,
    seen: FxHashSet<ClassLiteral<'db>>,
    pending: Vec<ClassLiteral<'db>>,
}

impl<'db> Writing<'_, 'db> {
    /// the row of `class`, if a class is one the runtime needs one for: a stub's, whose definition
    /// doesn't run, or one in a python file whose variance only a checker can infer
    fn row(&mut self, class: ClassLiteral<'db>) -> Option<String> {
        let db = self.db;
        let source = class.file(db).source_type(db);
        let variances = class_variances(db, class).filter(|_| !class.is_tuple(db));
        if !source.is_stub() && (source == PySourceType::BasedPython || variances.is_none()) {
            return None;
        }
        let params = class_params(db, class);
        let defaults = if params.is_empty() {
            "None".to_owned()
        } else {
            tuple(params.iter().map(|param| match param.default_type(db) {
                Some(default) => self.type_data(default, &params, 0),
                None => "...".to_owned(),
            }))
        };
        let protocol = class.into_protocol_class(db);
        let members = match &protocol {
            Some(protocol) => tuple(
                protocol
                    .interface(db)
                    .members(db)
                    .map(|member| python_str_literal(member.name())),
            ),
            None => "None".to_owned(),
        };
        let bases = tuple(
            class
                .explicit_bases(db)
                .iter()
                .filter_map(|base| self.base_data(*base, &params)),
        );
        let told_variances = variances.map_or_else(
            || "None".to_owned(),
            |variances| {
                tuple(
                    variances
                        .iter()
                        .copied()
                        .map(ArgVariance::code)
                        .map(|code| code.to_string()),
                )
            },
        );
        let methods = if source.is_stub() {
            self.methods_data(class, &params)
        } else {
            "()".to_owned()
        };
        Some(tuple(
            [
                python_str_literal(&class_path(db, class)),
                defaults,
                members,
                bases,
                told_variances,
                methods,
            ]
            .into_iter(),
        ))
    }

    fn base_data(
        &mut self,
        base: Type<'db>,
        params: &[BoundTypeVarInstance<'db>],
    ) -> Option<String> {
        match base {
            Type::GenericAlias(alias) => {
                Some(self.class_type_data(ClassType::Generic(alias), params, 0))
            }
            Type::ClassLiteral(class) => {
                if class.known(self.db) == Some(KnownClass::Object) {
                    return None;
                }
                self.pending.push(class);
                Some(python_str_literal(&class_path(self.db, class)))
            }
            // `Protocol`, `Generic` and the like, which the class says something else about
            _ => None,
        }
    }

    /// what each member of `class` a test can ask about is, other than what `object` gives it
    fn methods_data(
        &mut self,
        class: ClassLiteral<'db>,
        params: &[BoundTypeVarInstance<'db>],
    ) -> String {
        let (db, env) = (self.db, self.env);
        let mut members = Vec::new();
        let mut names = member_names_a_test_can_ask_about(db, class.file(db)).clone();
        if let Some(protocol) = class.into_protocol_class(db) {
            names.extend(
                protocol
                    .interface(db)
                    .members(db)
                    .map(|member| member.name().to_owned()),
            );
            names.sort_unstable();
            names.dedup();
        }
        for name in &names {
            let Place::Defined(defined) = class
                .identity_specialization(db)
                .class_member(db, env, name, MemberLookupPolicy::MRO_NO_OBJECT_FALLBACK)
                .place
            else {
                continue;
            };
            let ty = defined.ty;
            let ways = if ty.is_none(db) {
                "None".to_owned()
            } else if let Some(callables) = ty.try_upcast_to_callable(db, env) {
                let mut ways = Vec::new();
                for callable in &callables {
                    for signature in callable.signatures(db) {
                        ways.extend(self.way(signature, params, true));
                    }
                }
                tuple(ways.into_iter())
            } else {
                "()".to_owned()
            };
            members.push(tuple([python_str_literal(name), ways].into_iter()));
        }
        tuple(members.into_iter())
    }

    /// one way of calling, with positional arguments alone, past a receiver when it has one. a
    /// signature that needs a keyword isn't one of those
    fn way(
        &mut self,
        signature: &Signature<'db>,
        params: &[BoundTypeVarInstance<'db>],
        receiver: bool,
    ) -> Option<String> {
        let db = self.db;
        let returned = self.type_data(signature.return_ty, params, 0);
        let parameters = signature.parameters();
        if parameters.is_gradual() {
            let any = python_str_literal("typing:Any");
            return Some(format!("(0, (), {any}, {returned})"));
        }
        let mut required = 0;
        let mut positional = Vec::new();
        let mut rest = "None".to_owned();
        let mut skip = receiver;
        for parameter in parameters {
            if parameter.is_variadic() {
                skip = false;
                rest = self.type_data(parameter.annotated_type(), params, 0);
            } else if parameter.is_positional() {
                if std::mem::take(&mut skip) {
                    continue;
                }
                if parameter.default_type(db).is_none() {
                    required = positional.len() + 1;
                }
                positional.push(self.type_data(parameter.annotated_type(), params, 0));
            } else if parameter.is_keyword_only() && parameter.default_type(db).is_none() {
                return None;
            }
        }
        Some(format!(
            "({required}, {}, {rest}, {returned})",
            tuple(positional.into_iter())
        ))
    }

    /// python data for the instance type `ty`, with `params` the type parameters of the class being
    /// described. a stub's alias can grow each time it is opened, and past `depth` there is
    /// nothing to say that a test could use
    fn type_data(
        &mut self,
        ty: Type<'db>,
        params: &[BoundTypeVarInstance<'db>],
        depth: usize,
    ) -> String {
        let (db, env) = (self.db, self.env);
        if depth > 16 {
            return unseen();
        }
        let ty = ty.resolve_type_alias(db);
        if ty.is_none(db) {
            return "None".to_owned();
        }
        match ty {
            Type::TypeVar(typevar) if typevar.is_typing_self(db) => "-1".to_owned(),
            // one a method declares is what any call can solve it to
            Type::TypeVar(typevar) => params
                .iter()
                .position(|param| param.identity(db) == typevar.identity(db))
                .map_or_else(
                    || python_str_literal("typing:Any"),
                    |index| index.to_string(),
                ),
            Type::Dynamic(_) => python_str_literal("typing:Any"),
            // which relates as what it marks
            Type::Overlapping(overlapping) => {
                self.type_data(overlapping.type_argument(db), params, depth + 1)
            }
            Type::NominalInstance(instance) => {
                self.class_type_data(instance.class(db, env), params, depth)
            }
            Type::ProtocolInstance(protocol) => match protocol.class_origin(db) {
                Some(origin) => self.class_type_data(*origin, params, depth),
                None => unseen(),
            },
            Type::Union(union) => {
                let mut arms = vec![python_str_literal("|")];
                arms.extend(
                    union
                        .elements(db)
                        .iter()
                        .map(|element| self.type_data(*element, params, depth + 1)),
                );
                tuple(arms.into_iter())
            }
            Type::SubclassOf(subclass_of) => match subclass_of.subclass_of().into_class(db, env) {
                Some(class) => tuple(
                    [
                        python_str_literal("builtins:type"),
                        self.class_type_data(class, params, depth),
                    ]
                    .into_iter(),
                ),
                None => python_str_literal("builtins:type"),
            },
            Type::LiteralValue(literal) => {
                let value = match literal.kind() {
                    LiteralValueTypeKind::Bool(value) => {
                        (if value { "True" } else { "False" }).to_owned()
                    }
                    LiteralValueTypeKind::Int(value) => value.as_i64().to_string(),
                    LiteralValueTypeKind::String(value) => python_str_literal(value.value(db)),
                    LiteralValueTypeKind::Bytes(value) => python_bytes_literal(value.value(db)),
                    _ => return unseen(),
                };
                format!("({}, {value},)", python_str_literal("="))
            }
            _ => unseen(),
        }
    }

    fn class_type_data(
        &mut self,
        class: ClassType<'db>,
        params: &[BoundTypeVarInstance<'db>],
        depth: usize,
    ) -> String {
        let db = self.db;
        let literal = class.class_literal(db);
        self.pending.push(literal);
        let path = python_str_literal(&class_path(db, literal));
        let ClassType::Generic(alias) = class else {
            return path;
        };
        let specialization = alias.specialization(db);
        let mut parts = vec![path];
        if let Some(spec) = specialization.tuple(db) {
            match spec {
                TupleSpec::Fixed(fixed) => {
                    parts.extend(
                        fixed
                            .iter_all_elements()
                            .map(|element| self.type_data(element, params, depth + 1)),
                    );
                }
                TupleSpec::Variable(variable) => {
                    if variable.prefix_elements().is_empty()
                        && variable.suffix_elements().is_empty()
                    {
                        parts.push(self.type_data(
                            variable.variable().element_type(db),
                            params,
                            depth + 1,
                        ));
                    } else {
                        parts.push(unseen());
                    }
                    parts.push("...".to_owned());
                }
            }
        } else {
            parts.extend(
                specialization
                    .types(db)
                    .iter()
                    .map(|argument| self.type_data(*argument, params, depth + 1)),
            );
        }
        tuple(parts.into_iter())
    }
}

/// `module:qualified.name`
fn class_path<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> String {
    let module = file_to_module(db, db.program_file(class.file(db)).resolver_file(db))
        .map(|module| module.name(db).to_string())
        .unwrap_or_default();
    let mut name = class.qualified_name(db).components_within_file();
    name.push(exported_name(db, class));
    format!("{module}:{}", name.join("."))
}

/// the name the module of `class` binds it under at runtime: a stub can define a class under a
/// name of its own and export it as what the module calls it, as `_collections_abc` does with
/// `class AbstractSet` and `Set = AbstractSet`
fn exported_name<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> String {
    let name = class.name(db);
    let file = db.program_file(class.file(db));
    if let Some(exported) = dunder_all_names(db, file)
        && !exported.contains(name)
        && let Some(alias) = exported.iter().find(|candidate| {
            global_symbol(db, file, candidate)
                .place
                .ignore_possibly_undefined()
                .is_some_and(|ty| ty.as_class_literal() == Some(class))
        })
    {
        return alias.to_string();
    }
    name.to_string()
}

/// whether `ty` is what `typing.Self` reads as
pub(crate) fn is_typing_self<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    match ty {
        Type::TypeVar(typevar) => typevar.is_typing_self(db),
        Type::SpecialForm(form) => form == SpecialFormType::TypingSelf,
        _ => false,
    }
}

/// a type there is no saying at runtime, which names no class
fn unseen() -> String {
    python_str_literal("?")
}

fn tuple(elements: impl Iterator<Item = String>) -> String {
    let elements: Vec<String> = elements.collect();
    let trailing = if elements.len() == 1 { "," } else { "" };
    format!("({}{trailing})", elements.join(", "))
}
