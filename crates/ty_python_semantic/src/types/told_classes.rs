//! basedpython: what the runtime is told about the classes a stub describes
//!
//! a type test the checker leaves to the runtime asks whether one type argument is a subtype of
//! another, and what the checker reads of a class of the standard library is in no object the
//! runtime holds: `class str(Sequence[str])` is a `str` whose `__bases__` is `(object,)`, and
//! `class Iterable(Protocol[T])` is an abc that says nothing of being a protocol. so a module tells
//! the runtime about each such class it reads as a type, with what the checker read
//!
//! what is told is data, as `_by_told` in the runtime describes, so that it can be written ahead
//! of anything the module binds, and each name read as what it means where the class came from

use std::collections::BTreeMap;

use ruff_db::files::File;
use rustc_hash::FxHashSet;

use crate::Db;
use crate::ProgramEnvironment;
use crate::dunder_all::dunder_all_names;
use crate::place::global_symbol;
use crate::types::class::{ClassLiteral, ClassType};
use crate::types::class_base::ClassBase;
use crate::types::literal::LiteralValueTypeKind;
use crate::types::reified_infer::{python_bytes_literal, python_str_literal};
use crate::types::signatures::Signature;
use crate::types::special_form::SpecialFormType;
use crate::types::tuple::TupleSpec;
use crate::types::typevar::BoundTypeVarInstance;
use crate::types::{KnownClass, Type};
use ty_module_resolver::file_to_module;

/// what the runtime is told, a row of python data for each class by the path that names it
pub(crate) type ToldClasses = BTreeMap<String, String>;

/// everything the runtime has to be told to read `ty`, an instance type a module reads where
/// `file` runs, in which a class literal is what a final class's `type[C]` reads as
pub(crate) fn told_classes_of_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    ty: Type<'db>,
) -> ToldClasses {
    let mut telling = Telling::new(db, env, file);
    telling.part(ty);
    telling.rows()
}

/// everything the runtime has to be told to read `ty`, what a name that reads a class or a
/// subscript that specializes one reads as, a base of a class included
pub(crate) fn told_classes_of_class_read<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    ty: Type<'db>,
) -> ToldClasses {
    let mut telling = Telling::new(db, env, file);
    match ty {
        Type::ClassLiteral(class) => telling.class(class),
        _ => telling.part(ty),
    }
    telling.rows()
}

/// everything the runtime has to be told to read `class`, spelled by `file`
pub(crate) fn told_classes_of_class<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    class: ClassLiteral<'db>,
) -> ToldClasses {
    let mut telling = Telling::new(db, env, file);
    telling.class(class);
    telling.rows()
}

struct Telling<'a, 'db> {
    db: &'db dyn Db,
    env: &'a ProgramEnvironment<'db>,
    file: File,
    seen: FxHashSet<ClassLiteral<'db>>,
    /// in the order they were found, so that what is told doesn't depend on a hash
    classes: Vec<ClassLiteral<'db>>,
    /// the classes read as `type[C]`, where what calling one is can be asked about
    called: FxHashSet<ClassLiteral<'db>>,
    /// the types already read, which a recursive one comes back to
    parts: FxHashSet<Type<'db>>,
}

impl<'a, 'db> Telling<'a, 'db> {
    fn new(db: &'db dyn Db, env: &'a ProgramEnvironment<'db>, file: File) -> Self {
        Self {
            db,
            env,
            file,
            seen: FxHashSet::default(),
            classes: Vec::new(),
            called: FxHashSet::default(),
            parts: FxHashSet::default(),
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
                self.called.insert(class);
                self.class(class);
            }
            Type::GenericAlias(alias) => self.class_type(ClassType::Generic(alias)),
            Type::SubclassOf(subclass_of) => {
                if let Some(class) = subclass_of.subclass_of().into_class(db, env) {
                    self.called.insert(class.class_literal(db));
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
            Type::TypeAlias(_) => self.part(part.resolve_type_alias(db)),
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

    fn class(&mut self, class: ClassLiteral<'db>) {
        if !self.seen.insert(class) {
            return;
        }
        self.classes.push(class);
        for base in class.iter_mro(self.db) {
            if let ClassBase::Class(base) = base {
                self.class(base.class_literal(self.db));
            }
        }
        // the checker reads a `TypedDict` as a `Mapping[str, object]`
        if class.is_typed_dict(self.db)
            && let Some(mapping) = KnownClass::Mapping
                .to_class_literal(self.db, self.env)
                .as_class_literal()
        {
            self.class(mapping);
        }
    }

    fn rows(mut self) -> ToldClasses {
        let mut rows = BTreeMap::new();
        // a row can name a class no mro does, and that has one as well
        let mut index = 0;
        while let Some(class) = self.classes.get(index).copied() {
            index += 1;
            if !self.tells(class) {
                continue;
            }
            let (path, row) = self.row(class);
            rows.insert(path, row);
        }
        rows
    }

    /// whether the runtime has to be told about `class`, for what the checker reads of it that the
    /// runtime has no object for: a stub's generic base, or a protocol. a stub's other classes
    /// derive at runtime from what they say they do
    fn tells(&self, class: ClassLiteral<'db>) -> bool {
        let db = self.db;
        if self.called.contains(&class) {
            return true;
        }
        class.file(db).source_type(db).is_stub()
            && class.iter_mro(db).any(|base| {
                !matches!(
                    base,
                    ClassBase::Class(ClassType::NonGeneric(_)) | ClassBase::Dynamic(_)
                )
            })
    }

    fn row(&mut self, class: ClassLiteral<'db>) -> (String, String) {
        let db = self.db;
        let params: Vec<BoundTypeVarInstance<'db>> = class
            .generic_context(db)
            .map(|context| context.variables(db).collect())
            .unwrap_or_default();
        let defaults = if params.is_empty() {
            "None".to_owned()
        } else {
            tuple(params.iter().map(|param| match param.default_type(db) {
                Some(default) => self.type_data(default, &params),
                None => "...".to_owned(),
            }))
        };
        let members = match class.into_protocol_class(db) {
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
        let constructors = if self.called.contains(&class) {
            self.constructors_data(class, &params)
        } else {
            "()".to_owned()
        };
        let path = self.class_path(class);
        let row = format!(
            "({}, {defaults}, {members}, {bases}, {constructors})",
            python_str_literal(&path)
        );
        (path, row)
    }

    fn base_data(
        &mut self,
        base: Type<'db>,
        params: &[BoundTypeVarInstance<'db>],
    ) -> Option<String> {
        match base {
            Type::GenericAlias(alias) => {
                Some(self.class_type_data(ClassType::Generic(alias), params))
            }
            Type::ClassLiteral(class) => (class.known(self.db) != Some(KnownClass::Object))
                .then(|| python_str_literal(&self.class_path(class))),
            // `Protocol`, `Generic` and the like, which the class says something else about
            _ => None,
        }
    }

    /// what calling `class` is, for each way it can be called with positional arguments alone
    fn constructors_data(
        &mut self,
        class: ClassLiteral<'db>,
        params: &[BoundTypeVarInstance<'db>],
    ) -> String {
        let (db, env) = (self.db, self.env);
        let Some(callables) = Type::ClassLiteral(class).try_upcast_to_callable(db, env) else {
            return "()".to_owned();
        };
        let mut constructors = Vec::new();
        for callable in &callables {
            for signature in callable.signatures(db) {
                if let Some(constructor) = self.constructor_data(signature, params) {
                    constructors.push(constructor);
                }
            }
        }
        tuple(constructors.into_iter())
    }

    fn constructor_data(
        &mut self,
        signature: &Signature<'db>,
        params: &[BoundTypeVarInstance<'db>],
    ) -> Option<String> {
        let db = self.db;
        let returned = self.type_data(signature.return_ty, params);
        let parameters = signature.parameters();
        let any = "\"typing:Any\"".to_owned();
        if parameters.is_gradual() {
            return Some(format!("(0, (), {any}, {returned})"));
        }
        let mut required = 0;
        let mut positional = Vec::new();
        let mut rest = "None".to_owned();
        for parameter in parameters {
            if parameter.is_variadic() {
                rest = self.type_data(parameter.annotated_type(), params);
            } else if parameter.is_positional() {
                if parameter.default_type(db).is_none() {
                    required = positional.len() + 1;
                }
                positional.push(self.type_data(parameter.annotated_type(), params));
            } else if parameter.is_keyword_only() && parameter.default_type(db).is_none() {
                // a call with positional arguments alone isn't one of these
                return None;
            }
        }
        Some(format!(
            "({required}, {}, {rest}, {returned})",
            tuple(positional.into_iter())
        ))
    }

    /// python data for the instance type `ty`, with `params` the type parameters of the class
    /// being described
    fn type_data(&mut self, ty: Type<'db>, params: &[BoundTypeVarInstance<'db>]) -> String {
        let (db, env) = (self.db, self.env);
        let ty = ty.resolve_type_alias(db);
        if ty.is_none(db) {
            return "None".to_owned();
        }
        match ty {
            Type::TypeVar(typevar) => params
                .iter()
                .position(|param| param.identity(db) == typevar.identity(db))
                .map_or_else(unseen, |index| index.to_string()),
            Type::Dynamic(_) => python_str_literal("typing:Any"),
            Type::NominalInstance(instance) => {
                self.class_type_data(instance.class(db, env), params)
            }
            Type::ProtocolInstance(protocol) => match protocol.class_origin(db) {
                Some(origin) => self.class_type_data(*origin, params),
                None => unseen(),
            },
            Type::Union(union) => {
                let mut arms = vec![python_str_literal("|")];
                arms.extend(
                    union
                        .elements(db)
                        .iter()
                        .map(|element| self.type_data(*element, params)),
                );
                tuple(arms.into_iter())
            }
            Type::SubclassOf(subclass_of) => match subclass_of.subclass_of().into_class(db, env) {
                Some(class) => tuple(
                    [
                        python_str_literal("builtins:type"),
                        self.class_type_data(class, params),
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
    ) -> String {
        let db = self.db;
        let literal = class.class_literal(db);
        self.class(literal);
        let path = python_str_literal(&self.class_path(literal));
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
                            .map(|element| self.type_data(element, params)),
                    );
                }
                TupleSpec::Variable(variable) => {
                    if variable.prefix_elements().is_empty()
                        && variable.suffix_elements().is_empty()
                    {
                        parts.push(self.type_data(variable.variable().element_type(db), params));
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
                    .map(|argument| self.type_data(*argument, params)),
            );
        }
        tuple(parts.into_iter())
    }

    /// `module:qualified.name`, with a leading `:` for the module doing the telling, which is the
    /// one module whose name may not be what runs it: `__main__`
    fn class_path(&self, class: ClassLiteral<'db>) -> String {
        let db = self.db;
        let file = class.file(db);
        let module = if file == self.file {
            String::new()
        } else {
            file_to_module(db, db.program_file(file).resolver_file(db))
                .map(|module| module.name(db).to_string())
                .unwrap_or_default()
        };
        let mut name = class.qualified_name(db).components_within_file();
        name.push(self.exported_name(class));
        format!("{module}:{}", name.join("."))
    }
}

impl<'db> Telling<'_, 'db> {
    /// the name the module of `class` binds it under at runtime: a stub can define a class under
    /// a name of its own and export it as what the module calls it, as `_collections_abc` does
    /// with `class AbstractSet` and `Set = AbstractSet`
    fn exported_name(&self, class: ClassLiteral<'db>) -> String {
        let db = self.db;
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
