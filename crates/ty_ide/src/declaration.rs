//! what a declaration is: its kind, its name, and the dotted name it is reached by from outside
//! its module
//!
//! ```py
//! # shapes/base.py
//! class Circle:
//!     def __init__(self, radius: float) -> None:
//!         self.radius = radius
//!
//!     def area(self) -> float: ...
//! ```
//!
//! `Circle` is the class `shapes.base.Circle`, `area` the method `shapes.base.Circle.area`, and
//! `radius` the attribute `shapes.base.Circle.radius`, although it is assigned in `__init__`. a
//! name local to a function, a parameter among them, has no qualified name, and nor does a type
//! parameter: nothing outside the function or the generic reaches it

use ruff_db::files::FileRange;
use ruff_db::parsed::parsed_module;
use ruff_text_size::{Ranged, TextSize};
use ty_module_resolver::file_to_module;
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::place::PlaceExprRef;
use ty_python_core::scope::{FileScopeId, ScopeKind};
use ty_python_core::{ProgramFile, semantic_index};
use ty_python_semantic::types::ide_support::decorated_function;
use ty_python_semantic::{HasDefinition, ImportAliasResolution, ResolvedDefinition, SemanticModel};

use crate::goto::{Definitions, GotoTarget, find_goto_target};
use crate::{Db, NavigationTarget, RangedValue};

/// what kind of thing a declaration declares
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclaredKind {
    Module,
    Class,
    Function,
    /// a `def` in a class body
    Method,
    /// a `def` in a class body that its decorators make a property
    Property,
    /// a name a class body binds that is not a `def` or a class, or an attribute a method assigns
    /// on `self`
    Attribute,
    Variable,
    Parameter,
    TypeParameter,
    TypeAlias,
}

impl DeclaredKind {
    /// the kind as a reader names it
    pub const fn as_str(self) -> &'static str {
        match self {
            DeclaredKind::Module => "module",
            DeclaredKind::Class => "class",
            DeclaredKind::Function => "function",
            DeclaredKind::Method => "method",
            DeclaredKind::Property => "property",
            DeclaredKind::Attribute => "attribute",
            DeclaredKind::Variable => "variable",
            DeclaredKind::Parameter => "parameter",
            DeclaredKind::TypeParameter => "type parameter",
            DeclaredKind::TypeAlias => "type alias",
        }
    }
}

/// a declaration, described
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub kind: DeclaredKind,
    /// the declared name
    pub name: String,
    /// the module's dotted name, then each class the declaration is made in, then the name:
    /// `shapes.base.Circle.area`. `None` for a name local to a function, and for a declaration
    /// in a file no module is imported from
    pub qualified_name: Option<String>,
}

/// the declaration the name at `offset` refers to, or declares, with where it is
#[derive(Debug, Clone)]
pub struct DeclarationAt {
    pub declaration: Declaration,
    /// where the declaration is: the whole of it, and the name it declares
    pub target: NavigationTarget,
}

/// what the name at `offset` declares, or refers to: on `total_area` in `total_area(shapes)`
/// the function `shapes.util.total_area`, on `Circle` in `Circle(3)` the class rather than the
/// `__init__` the call runs. `None` where there is no name, and for a name whose definitions
/// declare different things
pub fn declaration_at(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
) -> Option<RangedValue<DeclarationAt>> {
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let model = SemanticModel::new(db, file);
    let goto_target = find_goto_target(&model, &parsed, offset)?;
    let declared = declared_definitions(&model, &goto_target)?;
    let declaration = single_declaration(db, &declared)?;
    let target = declared.into_navigation_targets(db).into_iter().next()?;
    Some(RangedValue {
        range: FileRange::new(file.file(db), goto_target.range()),
        value: DeclarationAt {
            declaration,
            target,
        },
    })
}

/// the definitions of what the name at `goto_target` declares, read off the name's own
/// definitions rather than off what a call of it runs: `Circle` in `Circle(3)` is the class,
/// whose `__init__` the call runs, and the name of a decorated `def` is the `def`, which a
/// decorator returning a function of its own would otherwise stand in for
pub(crate) fn declared_definitions<'db>(
    model: &SemanticModel<'db>,
    goto_target: &GotoTarget<'_>,
) -> Option<Definitions<'db>> {
    match goto_target {
        GotoTarget::Expression(_) | GotoTarget::Call { .. } => {
            goto_target.expression_definitions(model, ImportAliasResolution::ResolveAliases)
        }
        GotoTarget::FunctionDef(function) => {
            Some(Definitions::new(vec![ResolvedDefinition::Definition(
                function.definition(model),
            )]))
        }
        _ => goto_target.definitions(model, ImportAliasResolution::ResolveAliases),
    }
}

/// the one declaration `declared` makes, when its definitions all declare the same name the same
/// way: the bindings of a variable assigned twice are one variable
pub(crate) fn single_declaration(db: &dyn Db, declared: &Definitions<'_>) -> Option<Declaration> {
    let mut declarations = declared.iter().map(|resolved| declaration(db, resolved));
    let first = declarations.next()??;
    declarations
        .all(|other| other.as_ref() == Some(&first))
        .then_some(first)
}

/// what `resolved` declares. `None` for what is not a declaration of a name: an import that was
/// not followed to what it imports, or a range in a file that is not a definition
pub(crate) fn declaration(db: &dyn Db, resolved: &ResolvedDefinition<'_>) -> Option<Declaration> {
    match resolved {
        ResolvedDefinition::Module(file) => {
            let qualified_name = module_name(db, *file)?;
            Some(Declaration {
                kind: DeclaredKind::Module,
                name: qualified_name
                    .rsplit('.')
                    .next()
                    .unwrap_or(&qualified_name)
                    .to_string(),
                qualified_name: Some(qualified_name),
            })
        }
        ResolvedDefinition::Definition(definition) => definition_declaration(db, *definition),
        ResolvedDefinition::FileWithRange(_) => None,
    }
}

fn definition_declaration(db: &dyn Db, definition: Definition<'_>) -> Option<Declaration> {
    let file = definition.program_file(db);
    let index = semantic_index(db, file);
    let scope = definition.file_scope(db);
    let places = index.place_table(scope);
    let (name, on_self) = match places.place(definition.place(db)) {
        PlaceExprRef::Symbol(symbol) => (symbol.name().to_string(), false),
        // `self.radius = radius` in a method declares the class's attribute; an assignment to
        // any other object's attribute declares nothing of this scope's
        PlaceExprRef::Member(member) => (member.as_instance_attribute()?.to_string(), true),
    };
    let in_class = on_self || index.scope(scope).kind() == ScopeKind::Class;

    let kind = match definition.kind(db) {
        DefinitionKind::Import(_)
        | DefinitionKind::ImportFrom(_)
        | DefinitionKind::ImportFromSubmodule(_)
        | DefinitionKind::StarImport(_) => return None,
        DefinitionKind::Class(_) => DeclaredKind::Class,
        DefinitionKind::Function(_) if !in_class => DeclaredKind::Function,
        DefinitionKind::Function(_) => {
            if decorated_function(db, definition).is_some_and(|function| function.is_property()) {
                DeclaredKind::Property
            } else {
                DeclaredKind::Method
            }
        }
        DefinitionKind::TypeAlias(_) => DeclaredKind::TypeAlias,
        DefinitionKind::Parameter(_) | DefinitionKind::LambdaParameter(_) => {
            DeclaredKind::Parameter
        }
        DefinitionKind::TypeVar(_)
        | DefinitionKind::ParamSpec(_)
        | DefinitionKind::TypeVarTuple(_) => DeclaredKind::TypeParameter,
        _ if in_class => DeclaredKind::Attribute,
        _ => DeclaredKind::Variable,
    };

    // the scope the declared name lives in: a class's, for an attribute a method assigns
    let owner = if on_self {
        let class = index.class_definition_of_method(scope)?;
        Some((class.file_scope(db), class.name(db)?))
    } else {
        None
    };
    // a parameter, or a type parameter, is reached only through what it parameterizes
    let qualified_name = if matches!(kind, DeclaredKind::Parameter | DeclaredKind::TypeParameter) {
        None
    } else {
        let (start, owner_name) = owner.map_or((scope, None), |(scope, name)| (scope, Some(name)));
        containers(db, file, start).and_then(|mut containers| {
            containers.extend(owner_name);
            containers.push(name.clone());
            let module = module_name(db, file)?;
            Some(format!("{module}.{}", containers.join(".")))
        })
    };

    Some(Declaration {
        kind,
        name,
        qualified_name,
    })
}

/// a class, by its name and by the dotted name it is reached by
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassName {
    pub name: String,
    /// `shapes.base.Circle`. `None` for a class local to a function, and for one in a file no
    /// module is imported from
    pub qualified_name: Option<String>,
}

/// the class `resolved` declares, or the class it is a member of: `Circle` for the class
/// `Circle`, for its method `area` and for the `radius` its `__init__` assigns on `self` alike.
/// `None` for anything else, such as a module's function
pub(crate) fn declaring_class(db: &dyn Db, resolved: &ResolvedDefinition<'_>) -> Option<ClassName> {
    let ResolvedDefinition::Definition(definition) = resolved else {
        return None;
    };
    let definition = *definition;
    if matches!(definition.kind(db), DefinitionKind::Class(_)) {
        let declaration = definition_declaration(db, definition)?;
        return Some(ClassName {
            name: declaration.name,
            qualified_name: declaration.qualified_name,
        });
    }
    let file = definition.program_file(db);
    let index = semantic_index(db, file);
    let scope = definition.file_scope(db);
    if let PlaceExprRef::Member(member) = index.place_table(scope).place(definition.place(db)) {
        member.as_instance_attribute()?;
        let class = index.class_definition_of_method(scope)?;
        return declaring_class(db, &ResolvedDefinition::Definition(class));
    }
    if index.scope(scope).kind() != ScopeKind::Class {
        return None;
    }
    let module = parsed_module(db, file.python_file(db)).load(db);
    let name = scope.to_scope_id(db, file).name(db, &module).to_string();
    let qualified_name = containers(db, file, scope).and_then(|containers| {
        let module = module_name(db, file)?;
        Some(format!("{module}.{}", containers.join(".")))
    });
    Some(ClassName {
        name,
        qualified_name,
    })
}

/// the names of the classes `scope` is nested in, outermost first, `scope` itself included when
/// it is a class's. `None` when a function encloses it, which makes what it declares local
fn containers(db: &dyn Db, file: ProgramFile<'_>, scope: FileScopeId) -> Option<Vec<String>> {
    let index = semantic_index(db, file);
    let module = parsed_module(db, file.python_file(db)).load(db);
    let mut containers = Vec::new();
    for (id, ancestor) in index.ancestor_scopes(scope) {
        match ancestor.kind() {
            ScopeKind::Module => break,
            ScopeKind::Class => {
                containers.push(id.to_scope_id(db, file).name(db, &module).to_string());
            }
            // the scope of a generic class's or function's type parameters sits between it and
            // what encloses it, and adds no name
            ScopeKind::TypeParams => {}
            ScopeKind::Function
            | ScopeKind::Lambda
            | ScopeKind::Comprehension
            | ScopeKind::TypeAlias => return None,
        }
    }
    containers.reverse();
    Some(containers)
}

/// the dotted name `file` is imported by
fn module_name(db: &dyn Db, file: ProgramFile<'_>) -> Option<String> {
    file_to_module(db, file.resolver_file(db)).map(|module| module.name(db).to_string())
}
