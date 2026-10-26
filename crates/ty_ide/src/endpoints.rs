//! the HTTP endpoints a project serves: what an editor's *endpoints* view lists
//!
//! ```py
//! # app/items.py
//! router = APIRouter(prefix="/items")
//!
//! @router.get("/{item_id}")
//! def read_item(item_id: int) -> Item: ...
//!
//! # app/main.py
//! app = FastAPI()
//! app.include_router(items.router, prefix="/v1")
//! ```
//!
//! `read_item` serves `GET /v1/items/{item_id}` for the application `app.main.app`. every part of
//! that is read through what the code means rather than how it is spelled: `router.get` is a route
//! because it resolves to FastAPI's `APIRouter.get`, whatever the router is called and however it
//! was imported, the router `include_router` is handed is the one the route was declared on because
//! both names resolve to the same assignment, and the prefixes are what ty infers them to be, so a
//! prefix held in a constant is read as well as one written out
//!
//! django's routes are read the same way from the urlconf module `ROOT_URLCONF` names, or, in a
//! project whose settings do not name one, from every module with `urlpatterns` that no other
//! includes: an entry is a route when it is a call of `django.urls.path` or `re_path`, an
//! `include()` is followed to the module, the list or the module object it names, and a view built
//! by `as_view()` is handled by its class
//!
//! a route whose path, or a prefix it is mounted under, is not a string ty knows exactly is left
//! out rather than listed at a path it may not have

use ruff_db::files::FileRange;
use ruff_db::parsed::parsed_module;
use ruff_python_ast::visitor::source_order::{self, SourceOrderVisitor};
use ruff_python_ast::{self as ast};
use ruff_text_size::Ranged;
use rustc_hash::{FxHashMap, FxHashSet};
use ty_module_resolver::{ImportingFile, ModuleName, resolve_real_module};
use ty_python_core::ProgramFile;
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_semantic::types::ide_support::{module_value_file, string_literal_value};
use ty_python_semantic::{HasDefinition, ImportAliasResolution, ResolvedDefinition, SemanticModel};

use crate::declaration::declaration;
use crate::goto::{GotoTarget, navigation_target};
use crate::{Db, NavigationTarget};

/// the framework that serves an endpoint
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Framework {
    FastApi,
    Django,
}

impl Framework {
    /// the name a client sees
    pub const fn as_str(self) -> &'static str {
        match self {
            Framework::FastApi => "fastapi",
            Framework::Django => "django",
        }
    }
}

/// one route a project declares
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub framework: Framework,
    /// the HTTP methods the route serves, upper case: `["GET"]`. empty for a route that serves
    /// every method, as each of django's does
    pub methods: Vec<String>,
    /// the path it is served at, with every prefix it is mounted under, and its parameters as the
    /// route writes them: `/v1/items/{item_id}`
    pub path: String,
    /// what handles a request to it
    pub handler: EndpointTarget,
    /// where the route is declared: the decorator, or the call that adds it, `path(...)` among them
    pub declaration: FileRange,
    /// the application that serves it: a FastAPI application, or the urlconf module django starts
    /// at. `None` for a route on a router no application includes
    pub application: Option<EndpointTarget>,
}

/// a declaration an endpoint names: its qualified name, where it has one, and where it is
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointTarget {
    pub name: Option<String>,
    pub target: NavigationTarget,
}

/// every endpoint the project's files declare, ordered by path and then by method
pub fn endpoints(db: &dyn Db) -> Vec<Endpoint> {
    let mut scan = FastApiScan::default();
    for file in db.project().files(db).iter() {
        let file = db.program_file(file);
        scan.scan_file(db, file);
    }
    let mut found = scan.endpoints(db);
    found.extend(django_endpoints(db));
    found.sort_by(|a, b| {
        (
            &a.path,
            &a.methods,
            a.declaration.file(),
            a.declaration.range().start(),
        )
            .cmp(&(
                &b.path,
                &b.methods,
                b.declaration.file(),
                b.declaration.range().start(),
            ))
    });
    found
}

/// the FastAPI class that holds the methods declaring routes on an application
const FASTAPI_APPLICATION: &str = "fastapi.applications.FastAPI";

/// the FastAPI class that holds the methods declaring routes on a router
const FASTAPI_ROUTER: &str = "fastapi.routing.APIRouter";

/// the decorators that declare a route for one method, by the method they name
const METHOD_DECORATORS: [&str; 8] = [
    "get", "post", "put", "patch", "delete", "options", "head", "trace",
];

/// the decorator that declares a route for the methods it is given
const API_ROUTE_DECORATOR: &str = "api_route";

/// the call that adds a route for a function it is handed rather than decorates
const ADD_API_ROUTE: &str = "add_api_route";

/// the call that mounts a router's routes on an application or another router
const INCLUDE_ROUTER: &str = "include_router";

/// what FastAPI serves a route declared without `methods` for
const DEFAULT_METHOD: &str = "GET";

/// whether routes are declared on an application or on a router
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    Application,
    Router,
}

/// a route as declared, on the application or router `owner`
struct Route<'db> {
    owner: Definition<'db>,
    owner_kind: Owner,
    methods: Vec<String>,
    path: String,
    handler: ResolvedDefinition<'db>,
    declaration: FileRange,
}

/// a router mounted on an application or another router
struct Include<'db> {
    parent: Definition<'db>,
    parent_kind: Owner,
    router: Definition<'db>,
    prefix: String,
}

#[derive(Default)]
struct FastApiScan<'db> {
    routes: Vec<Route<'db>>,
    includes: Vec<Include<'db>>,
}

impl<'db> FastApiScan<'db> {
    fn scan_file(&mut self, db: &'db dyn Db, file: ProgramFile<'db>) {
        let parsed = parsed_module(db, file.python_file(db)).load(db);
        let mut visitor = FastApiVisitor {
            db,
            model: SemanticModel::new(db, file),
            file,
            scan: self,
        };
        visitor.visit_body(parsed.suite());
    }

    /// each route at every path an application serves it at, and a route on a router no
    /// application includes at the path the router gives it
    fn endpoints(&self, db: &'db dyn Db) -> Vec<Endpoint> {
        let mut found = Vec::new();
        let mut prefixes = FxHashMap::default();
        for route in &self.routes {
            let handler = target(db, &route.handler);
            let mounted = match route.owner_kind {
                Owner::Application => vec![(Some(route.owner), route.path.clone())],
                Owner::Router => {
                    let Some(own) = router_prefix(db, &mut prefixes, route.owner) else {
                        continue;
                    };
                    self.mounted(
                        db,
                        &mut prefixes,
                        route.owner,
                        format!("{own}{}", route.path),
                        &mut FxHashSet::default(),
                    )
                }
            };
            for (application, path) in mounted {
                found.push(Endpoint {
                    framework: Framework::FastApi,
                    methods: route.methods.clone(),
                    path,
                    handler: handler.clone(),
                    declaration: route.declaration,
                    application: application.map(|application| {
                        target(db, &ResolvedDefinition::Definition(application))
                    }),
                });
            }
        }
        found
    }

    /// every application `router` is mounted on, directly or through other routers, with the
    /// path that serves `path` there: FastAPI copies a router's routes into what includes it,
    /// under the include's prefix and the including router's own. a router no application
    /// includes is served nowhere, and answers itself with no application
    fn mounted(
        &self,
        db: &'db dyn Db,
        prefixes: &mut FxHashMap<Definition<'db>, Option<String>>,
        router: Definition<'db>,
        path: String,
        visiting: &mut FxHashSet<Definition<'db>>,
    ) -> Vec<(Option<Definition<'db>>, String)> {
        // a router included, around a loop, in itself is mounted nowhere new
        if !visiting.insert(router) {
            return Vec::new();
        }
        let mut found = Vec::new();
        let mut included = false;
        for include in self
            .includes
            .iter()
            .filter(|include| include.router == router)
        {
            included = true;
            let path = format!("{}{path}", include.prefix);
            match include.parent_kind {
                Owner::Application => found.push((Some(include.parent), path)),
                Owner::Router => {
                    let Some(own) = router_prefix(db, prefixes, include.parent) else {
                        continue;
                    };
                    found.extend(self.mounted(
                        db,
                        prefixes,
                        include.parent,
                        format!("{own}{path}"),
                        visiting,
                    ));
                }
            }
        }
        visiting.remove(&router);
        if !included {
            found.push((None, path));
        }
        found
    }
}

struct FastApiVisitor<'a, 'db> {
    db: &'db dyn Db,
    model: SemanticModel<'db>,
    file: ProgramFile<'db>,
    scan: &'a mut FastApiScan<'db>,
}

impl<'db> FastApiVisitor<'_, 'db> {
    /// the FastAPI method `function` resolves to, with whether it is an application's or a
    /// router's: `router.get` is the router's `get` for an `APIRouter` however it is spelled
    fn fastapi_method(&self, function: &ast::ExprAttribute) -> Option<Owner> {
        let db = self.db;
        let definitions = GotoTarget::Expression(ast::ExprRef::Attribute(function))
            .expression_definitions(&self.model, ImportAliasResolution::ResolveAliases)?;
        let mut owners = definitions.iter().map(|resolved| {
            let qualified = declaration(db, resolved)?.qualified_name?;
            let (class, method) = qualified.rsplit_once('.')?;
            if method != function.attr.as_str() {
                return None;
            }
            match class {
                FASTAPI_APPLICATION => Some(Owner::Application),
                FASTAPI_ROUTER => Some(Owner::Router),
                _ => None,
            }
        });
        let owner = owners.next()??;
        owners.all(|other| other == Some(owner)).then_some(owner)
    }

    /// the one assignment `receiver` names, which is how two mentions of an application or a
    /// router are known to be the same one
    fn assignment(&self, receiver: &ast::Expr) -> Option<Definition<'db>> {
        let definitions = GotoTarget::Expression(receiver.into())
            .expression_definitions(&self.model, ImportAliasResolution::ResolveAliases)?;
        let mut assignments = definitions.iter().map(|resolved| match resolved {
            ResolvedDefinition::Definition(definition) => Some(*definition),
            _ => None,
        });
        let first = assignments.next()??;
        assignments
            .all(|other| other == Some(first))
            .then_some(first)
    }

    /// the methods a route declared with `methods=` serves, or GET without it. `None` where
    /// `methods` is something other than a display of strings ty knows
    fn methods(&self, arguments: &ast::Arguments) -> Option<Vec<String>> {
        let Some(methods) = arguments.find_keyword("methods") else {
            return Some(vec![DEFAULT_METHOD.to_string()]);
        };
        let elements = match &methods.value {
            ast::Expr::List(list) => &list.elts,
            ast::Expr::Tuple(tuple) => &tuple.elts,
            ast::Expr::Set(set) => &set.elts,
            _ => return None,
        };
        elements
            .iter()
            .map(|element| {
                string_literal_value(&self.model, element).map(|method| method.to_uppercase())
            })
            .collect()
    }

    /// the route a decorator declares on `function`
    fn decorator(&mut self, decorator: &ast::Decorator, function: &ast::StmtFunctionDef) {
        let ast::Expr::Call(call) = &decorator.expression else {
            return;
        };
        let ast::Expr::Attribute(callee) = call.func.as_ref() else {
            return;
        };
        let name = callee.attr.as_str();
        let methods = if METHOD_DECORATORS.contains(&name) {
            vec![name.to_uppercase()]
        } else if name == API_ROUTE_DECORATOR {
            let Some(methods) = self.methods(&call.arguments) else {
                return;
            };
            methods
        } else {
            return;
        };
        let handler = ResolvedDefinition::Definition(function.definition(&self.model));
        self.route(call, callee, methods, handler);
    }

    /// `add_api_route(path, endpoint, methods=...)`, which declares a route for a function
    /// without decorating it
    fn add_api_route(&mut self, call: &ast::ExprCall, callee: &ast::ExprAttribute) {
        let Some(endpoint) = call.arguments.find_argument_value("endpoint", 1) else {
            return;
        };
        let Some(handler) = self.assignment(endpoint) else {
            return;
        };
        let Some(methods) = self.methods(&call.arguments) else {
            return;
        };
        self.route(
            call,
            callee,
            methods,
            ResolvedDefinition::Definition(handler),
        );
    }

    fn route(
        &mut self,
        call: &ast::ExprCall,
        callee: &ast::ExprAttribute,
        methods: Vec<String>,
        handler: ResolvedDefinition<'db>,
    ) {
        let Some(owner_kind) = self.fastapi_method(callee) else {
            return;
        };
        let Some(owner) = self.assignment(&callee.value) else {
            return;
        };
        let Some(path) = call
            .arguments
            .find_argument_value("path", 0)
            .and_then(|path| string_literal_value(&self.model, path))
        else {
            return;
        };
        self.scan.routes.push(Route {
            owner,
            owner_kind,
            methods,
            path,
            handler,
            declaration: FileRange::new(self.file.file(self.db), call.range()),
        });
    }

    /// `include_router(router, prefix=...)`, which mounts a router on what it is called on
    fn include_router(&mut self, call: &ast::ExprCall, callee: &ast::ExprAttribute) {
        let Some(parent_kind) = self.fastapi_method(callee) else {
            return;
        };
        let (Some(parent), Some(router)) = (
            self.assignment(&callee.value),
            call.arguments
                .find_argument_value("router", 0)
                .and_then(|router| self.assignment(router)),
        ) else {
            return;
        };
        let prefix = match call.arguments.find_keyword("prefix") {
            None => String::new(),
            Some(prefix) => match string_literal_value(&self.model, &prefix.value) {
                Some(prefix) => prefix,
                None => return,
            },
        };
        self.scan.includes.push(Include {
            parent,
            parent_kind,
            router,
            prefix,
        });
    }
}

impl<'a> SourceOrderVisitor<'a> for FastApiVisitor<'_, '_> {
    fn visit_stmt(&mut self, statement: &'a ast::Stmt) {
        if let ast::Stmt::FunctionDef(function) = statement {
            for decorator in &function.decorator_list {
                self.decorator(decorator, function);
            }
        }
        source_order::walk_stmt(self, statement);
    }

    fn visit_expr(&mut self, expression: &'a ast::Expr) {
        if let ast::Expr::Call(call) = expression
            && let ast::Expr::Attribute(callee) = call.func.as_ref()
        {
            match callee.attr.as_str() {
                ADD_API_ROUTE => self.add_api_route(call, callee),
                INCLUDE_ROUTER => self.include_router(call, callee),
                _ => {}
            }
        }
        source_order::walk_expr(self, expression);
    }
}

/// the prefix a router declares its routes under, `APIRouter(prefix="/items")`, read once per
/// router. empty for a router built without one, and `None` for one whose prefix ty does not know
/// exactly, or which is not built where it is assigned
fn router_prefix<'db>(
    db: &'db dyn Db,
    prefixes: &mut FxHashMap<Definition<'db>, Option<String>>,
    router: Definition<'db>,
) -> Option<String> {
    prefixes
        .entry(router)
        .or_insert_with(|| {
            let file = router.program_file(db);
            let parsed = parsed_module(db, file.python_file(db)).load(db);
            let value = match router.kind(db) {
                DefinitionKind::Assignment(assignment) => assignment.value(&parsed),
                DefinitionKind::AnnotatedAssignment(assignment) => assignment.value(&parsed)?,
                _ => return None,
            };
            let ast::Expr::Call(call) = value else {
                return None;
            };
            match call.arguments.find_keyword("prefix") {
                None => Some(String::new()),
                Some(prefix) => string_literal_value(&SemanticModel::new(db, file), &prefix.value),
            }
        })
        .clone()
}

/// the functions an entry of `urlpatterns` is built with, by what they resolve to
const DJANGO_PATH: &str = "django.urls.conf.path";
const DJANGO_RE_PATH: &str = "django.urls.conf.re_path";
const DJANGO_INCLUDE: &str = "django.urls.conf.include";

/// the method a class-based view is turned into a view function with
const DJANGO_AS_VIEW: &str = "django.views.generic.base.View.as_view";

/// what a urlconf module holds its routes in
const URLPATTERNS: &str = "urlpatterns";

/// the routes django serves, from the urlconf module `ROOT_URLCONF` names or, without one, from
/// every module of the project with `urlpatterns` that no other includes
fn django_endpoints(db: &dyn Db) -> Vec<Endpoint> {
    let roots = match *crate::django_template::django_root_urlconf(db, db.project()) {
        Some(root) => vec![db.program_file(root)],
        None => db
            .project()
            .files(db)
            .iter()
            .map(|file| db.program_file(file))
            .filter(|file| binds_urlpatterns(db, *file))
            .collect(),
    };
    let mut walks: Vec<(ProgramFile<'_>, DjangoWalk<'_>)> = roots
        .into_iter()
        .map(|root| {
            let mut walk = DjangoWalk {
                db,
                application: target(db, &ResolvedDefinition::Module(root)),
                found: Vec::new(),
                included: FxHashSet::default(),
                walking: Vec::new(),
            };
            walk.module(root, &[]);
            (root, walk)
        })
        .collect();
    let included: FxHashSet<ProgramFile<'_>> = walks
        .iter()
        .flat_map(|(_, walk)| walk.included.iter().copied())
        .collect();
    if walks.len() > 1 {
        walks.retain(|(root, _)| !included.contains(root));
    }
    walks.into_iter().flat_map(|(_, walk)| walk.found).collect()
}

/// whether `file` assigns `urlpatterns` at its top level
fn binds_urlpatterns(db: &dyn Db, file: ProgramFile<'_>) -> bool {
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    parsed
        .suite()
        .iter()
        .any(|statement| urlpatterns_value(statement).is_some())
}

/// the display a top-level statement assigns or adds to `urlpatterns`
fn urlpatterns_value(statement: &ast::Stmt) -> Option<&ast::Expr> {
    let (target, value) = match statement {
        ast::Stmt::Assign(assign) => match assign.targets.as_slice() {
            [target] => (target, assign.value.as_ref()),
            _ => return None,
        },
        ast::Stmt::AnnAssign(assign) => (assign.target.as_ref(), assign.value.as_deref()?),
        ast::Stmt::AugAssign(assign) if assign.op == ast::Operator::Add => {
            (assign.target.as_ref(), assign.value.as_ref())
        }
        _ => return None,
    };
    (target.as_name_expr()?.id.as_str() == URLPATTERNS).then_some(value)
}

/// a part of a route's pattern, as `path()` or `re_path()` writes it
#[derive(Clone)]
struct RoutePart {
    text: String,
    regex: bool,
}

/// the url tree walked from one urlconf module
struct DjangoWalk<'db> {
    db: &'db dyn Db,
    application: EndpointTarget,
    found: Vec<Endpoint>,
    /// every module an `include()` reached
    included: FxHashSet<ProgramFile<'db>>,
    /// the modules whose patterns are being walked, outermost first: one that includes a module
    /// among them includes itself, whose routes would nest under themselves without end
    walking: Vec<ProgramFile<'db>>,
}

impl<'db> DjangoWalk<'db> {
    fn module(&mut self, file: ProgramFile<'db>, prefix: &[RoutePart]) {
        if self.walking.contains(&file) {
            return;
        }
        self.walking.push(file);
        let db = self.db;
        let parsed = parsed_module(db, file.python_file(db)).load(db);
        let model = SemanticModel::new(db, file);
        for statement in parsed.suite() {
            if let Some(patterns) = urlpatterns_value(statement) {
                self.patterns(&model, file, patterns, prefix);
            }
        }
        self.walking.pop();
    }

    /// the entries of a list or tuple of patterns
    fn patterns(
        &mut self,
        model: &SemanticModel<'db>,
        file: ProgramFile<'db>,
        patterns: &ast::Expr,
        prefix: &[RoutePart],
    ) {
        let entries = match patterns {
            ast::Expr::List(list) => &list.elts,
            ast::Expr::Tuple(tuple) => &tuple.elts,
            _ => return,
        };
        for entry in entries {
            self.entry(model, file, entry, prefix);
        }
    }

    fn entry(
        &mut self,
        model: &SemanticModel<'db>,
        file: ProgramFile<'db>,
        entry: &ast::Expr,
        prefix: &[RoutePart],
    ) {
        let ast::Expr::Call(call) = entry else {
            return;
        };
        let regex = match callee(self.db, model, call).as_deref() {
            Some(DJANGO_PATH) => false,
            Some(DJANGO_RE_PATH) => true,
            _ => return,
        };
        let (Some(route), Some(view)) = (
            call.arguments
                .find_argument_value("route", 0)
                .and_then(|route| string_literal_value(model, route)),
            call.arguments.find_argument_value("view", 1),
        ) else {
            return;
        };
        let mut parts = prefix.to_vec();
        parts.push(RoutePart { text: route, regex });

        if let ast::Expr::Call(view_call) = view {
            match callee(self.db, model, view_call).as_deref() {
                Some(DJANGO_INCLUDE) => {
                    if let Some(included) = view_call.arguments.find_argument_value("arg", 0) {
                        self.include(model, file, included, &parts);
                    }
                    return;
                }
                Some(DJANGO_AS_VIEW) => {
                    if let ast::Expr::Attribute(as_view) = view_call.func.as_ref()
                        && let Some(class) = single_definition(model, &as_view.value)
                    {
                        self.route(file, call, &parts, &class);
                    }
                    return;
                }
                _ => return,
            }
        }
        if let Some(handler) = single_definition(model, view) {
            self.route(file, call, &parts, &handler);
        }
    }

    /// what `include()` mounts: a module named by a string or by the module itself, a list of
    /// patterns, or a `(patterns, app_name)` pair
    fn include(
        &mut self,
        model: &SemanticModel<'db>,
        file: ProgramFile<'db>,
        included: &ast::Expr,
        prefix: &[RoutePart],
    ) {
        let db = self.db;
        let included = match included {
            ast::Expr::Tuple(pair) => match pair.elts.first() {
                Some(patterns) => patterns,
                None => return,
            },
            other => other,
        };
        if matches!(included, ast::Expr::List(_) | ast::Expr::Tuple(_)) {
            self.patterns(model, file, included, prefix);
            return;
        }
        let module = if let Some(name) = string_literal_value(model, included) {
            ModuleName::new(&name)
                .and_then(|name| {
                    resolve_real_module(
                        db,
                        ImportingFile::File(file.file(db), file.resolver_environment(db)),
                        &name,
                    )
                })
                .and_then(|module| module.file(db))
                .map(|module| db.program_file(module))
        } else {
            module_value_file(model, included).map(|module| db.program_file(module))
        };
        if let Some(module) = module {
            self.included.insert(module);
            self.module(module, prefix);
        }
    }

    fn route(
        &mut self,
        file: ProgramFile<'db>,
        call: &ast::ExprCall,
        parts: &[RoutePart],
        handler: &ResolvedDefinition<'db>,
    ) {
        // django matches a route against the path without its leading slash, and a regular
        // expression's anchors say where the part it matches starts and stops, which joined
        // into one path they no longer do
        let path = parts
            .iter()
            .map(|part| {
                if part.regex {
                    let text = part.text.strip_prefix('^').unwrap_or(&part.text);
                    text.strip_suffix('$').unwrap_or(text)
                } else {
                    &part.text
                }
            })
            .collect::<String>();
        self.found.push(Endpoint {
            framework: Framework::Django,
            methods: Vec::new(),
            path: format!("/{path}"),
            handler: target(self.db, handler),
            declaration: FileRange::new(file.file(self.db), call.range()),
            application: Some(self.application.clone()),
        });
    }
}

/// the qualified name of what `call` calls, where every definition of it agrees
fn callee(db: &dyn Db, model: &SemanticModel<'_>, call: &ast::ExprCall) -> Option<String> {
    let definitions = GotoTarget::Expression(call.func.as_ref().into())
        .expression_definitions(model, ImportAliasResolution::ResolveAliases)?;
    let mut names = definitions.iter().map(|resolved| {
        declaration(db, resolved).and_then(|declaration| declaration.qualified_name)
    });
    let first = names.next()??;
    names
        .all(|other| other.as_ref() == Some(&first))
        .then_some(first)
}

/// the one thing `expr` names
fn single_definition<'db>(
    model: &SemanticModel<'db>,
    expr: &ast::Expr,
) -> Option<ResolvedDefinition<'db>> {
    let definitions = GotoTarget::Expression(expr.into())
        .expression_definitions(model, ImportAliasResolution::ResolveAliases)?;
    match definitions.iter().as_slice() {
        [single] => Some(single.clone()),
        _ => None,
    }
}

fn target(db: &dyn Db, definition: &ResolvedDefinition<'_>) -> EndpointTarget {
    EndpointTarget {
        name: declaration(db, definition).and_then(|declaration| declaration.qualified_name),
        target: navigation_target(db, definition),
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use ruff_db::source::source_text;
    use ruff_source_file::LineIndex;
    use ruff_text_size::Ranged;

    use crate::tests::CursorTest;

    const FASTAPI_ROUTING: &str = "\
from typing import Any, Callable

Decorator = Callable[[Callable[..., Any]], Callable[..., Any]]

class APIRouter:
    def __init__(self, *, prefix: str = '') -> None: ...
    def get(self, path: str) -> Decorator: ...
    def post(self, path: str) -> Decorator: ...
    def delete(self, path: str) -> Decorator: ...
    def api_route(self, path: str, *, methods: list[str] | None = None) -> Decorator: ...
    def add_api_route(
        self, path: str, endpoint: Callable[..., Any], *, methods: list[str] | None = None
    ) -> None: ...
    def include_router(self, router: APIRouter, *, prefix: str = '') -> None: ...
";

    const FASTAPI_APPLICATIONS: &str = "\
from typing import Any, Callable

from fastapi.routing import APIRouter, Decorator

class FastAPI:
    def get(self, path: str) -> Decorator: ...
    def post(self, path: str) -> Decorator: ...
    def add_api_route(
        self, path: str, endpoint: Callable[..., Any], *, methods: list[str] | None = None
    ) -> None: ...
    def include_router(self, router: APIRouter, *, prefix: str = '') -> None: ...
";

    fn test(sources: &[(&str, &str)]) -> CursorTest {
        let mut builder = CursorTest::builder();
        builder
            .source(
                "fastapi/__init__.py",
                "from .applications import FastAPI as FastAPI\nfrom .routing import APIRouter as APIRouter\n",
            )
            .source("fastapi/routing.py", FASTAPI_ROUTING)
            .source("fastapi/applications.py", FASTAPI_APPLICATIONS);
        for (path, source) in sources {
            builder.source(*path, *source);
        }
        builder.build()
    }

    impl CursorTest {
        fn endpoints(&self) -> String {
            super::endpoints(&self.db)
                .iter()
                .map(|endpoint| {
                    let file = endpoint.declaration.file();
                    let source = source_text(&self.db, file);
                    let line = LineIndex::from_source_text(&source)
                        .line_index(endpoint.declaration.range().start());
                    format!(
                        "{} {} {} -> {} declared at {}:{line} for {}",
                        endpoint.framework.as_str(),
                        endpoint.methods.join("|"),
                        endpoint.path,
                        endpoint.handler.name.as_deref().unwrap_or("-"),
                        file.path(&self.db).as_str(),
                        endpoint
                            .application
                            .as_ref()
                            .map_or("nothing", |application| application
                                .name
                                .as_deref()
                                .unwrap_or("-")),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    #[test]
    fn a_route_is_served_at_every_prefix_it_is_mounted_under() {
        let test = test(&[
            (
                "app/items.py",
                "\
from fastapi import APIRouter

PREFIX = '/items'
router = APIRouter(prefix=PREFIX)

@router.get('/{item_id}')
def read_item(item_id: int) -> int:
    return item_id

@router.api_route('/', methods=['get', 'POST'])
def items() -> None: ...
",
            ),
            (
                "app/users.py",
                "\
from fastapi import APIRouter

users_router = APIRouter(prefix='/users')
admin = APIRouter()

@admin.delete('/{user_id}')
def remove(user_id: int) -> None: ...

users_router.include_router(admin, prefix='/admin')
",
            ),
            (
                "app/main.py",
                "\
from fastapi import FastAPI as Application
from app import items
from app.users import users_router

app = Application()
app.include_router(items.router, prefix='/v1')
app.include_router(users_router)

@app.get('/health')
def health() -> str: ...

def ping() -> str: ...

app.add_api_route('/ping', ping, methods=['HEAD'])
<CURSOR>",
            ),
        ]);
        assert_snapshot!(test.endpoints(), @"
        fastapi GET /health -> app.main.health declared at /app/main.py:9 for app.main.app
        fastapi HEAD /ping -> app.main.ping declared at /app/main.py:14 for app.main.app
        fastapi DELETE /users/admin/{user_id} -> app.users.remove declared at /app/users.py:6 for app.main.app
        fastapi GET|POST /v1/items/ -> app.items.items declared at /app/items.py:10 for app.main.app
        fastapi GET /v1/items/{item_id} -> app.items.read_item declared at /app/items.py:6 for app.main.app
        ");
    }

    #[test]
    fn a_route_on_a_router_nothing_includes_is_served_by_nothing() {
        let test = test(&[(
            "main.py",
            "\
from fastapi import APIRouter

orphan = APIRouter(prefix='/orphan')

@orphan.post('/x')
def x() -> None: ...
<CURSOR>",
        )]);
        assert_snapshot!(test.endpoints(), @"fastapi POST /orphan/x -> main.x declared at /main.py:5 for nothing");
    }

    #[test]
    fn a_method_named_like_a_route_is_not_one() {
        let test = test(&[(
            "main.py",
            "\
from typing import Any, Callable

class Registry:
    def get(self, path: str) -> Callable[[Callable[..., Any]], Callable[..., Any]]: ...

registry = Registry()

@registry.get('/nope')
def nope() -> None: ...
<CURSOR>",
        )]);
        assert_snapshot!(test.endpoints(), @"");
    }

    #[test]
    fn a_path_that_is_not_known_exactly_is_left_out() {
        let test = test(&[(
            "main.py",
            "\
from fastapi import FastAPI

app = FastAPI()

def route(path: str) -> None:
    @app.get(path)
    def handler() -> None: ...
<CURSOR>",
        )]);
        assert_snapshot!(test.endpoints(), @"");
    }

    fn django_test(sources: &[(&str, &str)]) -> CursorTest {
        let mut builder = CursorTest::builder();
        builder
            .source(
                "django/urls/__init__.py",
                "from .conf import include as include, path as path, re_path as re_path\n",
            )
            .source(
                "django/urls/conf.py",
                "\
from typing import Any

def include(arg: Any) -> Any: ...
def path(route: str, view: Any, name: str | None = None) -> Any: ...
def re_path(route: str, view: Any, name: str | None = None) -> Any: ...
",
            )
            .source(
                "django/views/generic/base.py",
                "\
from typing import Any, Callable

class View:
    @classmethod
    def as_view(cls) -> Callable[..., Any]: ...
",
            );
        for (path, source) in sources {
            builder.source(*path, *source);
        }
        builder.build()
    }

    #[test]
    fn a_django_route_is_served_behind_every_include_it_is_mounted_under() {
        let test = django_test(&[
            (
                "blog/views.py",
                "\
from django.views.generic.base import View

def index(request: object) -> None: ...
def archive(request: object, year: str) -> None: ...

class PostDetail(View): ...
",
            ),
            (
                "blog/urls.py",
                "\
from django.urls import path, re_path

from . import views

urlpatterns = [
    path('', views.index),
    path('<int:pk>/', views.PostDetail.as_view()),
]
urlpatterns += [re_path(r'^archive/(?P<year>[0-9]{4})/$', views.archive)]
",
            ),
            (
                "mysite/urls.py",
                "\
from django.urls import include, path

import blog.urls
from blog import views

urlpatterns = [
    path('blog/', include('blog.urls')),
    path('again/', include(blog.urls)),
    path('inline/', include([path('index/', views.index)])),
]
<CURSOR>",
            ),
        ]);
        assert_snapshot!(test.endpoints(), @"
        django  /again/ -> blog.views.index declared at /blog/urls.py:6 for mysite.urls
        django  /again/<int:pk>/ -> blog.views.PostDetail declared at /blog/urls.py:7 for mysite.urls
        django  /again/archive/(?P<year>[0-9]{4})/ -> blog.views.archive declared at /blog/urls.py:9 for mysite.urls
        django  /blog/ -> blog.views.index declared at /blog/urls.py:6 for mysite.urls
        django  /blog/<int:pk>/ -> blog.views.PostDetail declared at /blog/urls.py:7 for mysite.urls
        django  /blog/archive/(?P<year>[0-9]{4})/ -> blog.views.archive declared at /blog/urls.py:9 for mysite.urls
        django  /inline/index/ -> blog.views.index declared at /mysite/urls.py:9 for mysite.urls
        ");
    }

    /// a urlconf that includes itself mounts its own routes under itself without end, which no
    /// request can reach the bottom of, so the include that closes the cycle is left out
    #[test]
    fn a_urlconf_that_includes_itself_is_walked_once() {
        let test = django_test(&[(
            "mysite/urls.py",
            "\
from django.urls import include, path

def index(request: object) -> None: ...

urlpatterns = [
    path('again/', include('mysite.urls')),
    path('a/', include([path('b/', include([path('c/', index)]))])),
]
<CURSOR>",
        )]);
        assert_snapshot!(test.endpoints(), @"django  /a/b/c/ -> mysite.urls.index declared at /mysite/urls.py:7 for mysite.urls");
    }

    #[test]
    fn a_call_named_like_path_is_not_a_route() {
        let test = django_test(&[(
            "urls.py",
            "\
def path(route: str, view: object) -> object: ...
def index() -> None: ...

urlpatterns = [path('nope/', index)]
<CURSOR>",
        )]);
        assert_snapshot!(test.endpoints(), @"");
    }
}
