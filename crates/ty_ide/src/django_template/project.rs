//! the django definitions a template needs from the project's python source
//!
//! everything a template can say that isn't a builtin comes from somewhere in
//! the project: `{% load %}`ed tags and filters from its `templatetags` modules,
//! `{% url %}` names from its url configuration, `{% extends %}` targets from its
//! template directories, and the variables in `{{ }}` from whichever view renders
//! the template. this module finds all of it.
//!
//! each source file is scanned by its own salsa query, and the project-wide
//! answers are the union of those. that keeps an edit to one view from re-walking
//! the whole project — and since the scans run off `parsed_module`, which the
//! type checker has already populated for every project file, the ast work is
//! shared rather than duplicated.

use std::sync::Mutex;

use compact_str::{CompactString, ToCompactString};
use ruff_db::files::{File, FilePath, system_path_to_file};
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_db::system::walk_directory::WalkState;
use ruff_db::system::{FileType, SystemPath, SystemPathBuf};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_project::{Db, Project};
use ty_python_semantic::SemanticModel;
use ty_python_semantic::types::ide_support::{
    ImportAliasResolution, definitions_for_attribute, definitions_for_name,
};

/// the directory name django's app-directories template loader looks in
const TEMPLATE_DIRECTORY: &str = "templates";

/// the directory name django's `staticfiles` app-directories finder looks in
const STATIC_DIRECTORY: &str = "static";

/// the package name a project's template tag libraries live in
const TEMPLATETAGS_PACKAGE: &str = "templatetags";

/// the functions that render a named template with a context
pub(crate) const CONTEXT_CALLEES: &[&str] = &["render", "TemplateResponse"];

/// the class attribute a generic view names its template with
pub(crate) const TEMPLATE_NAME_ATTRIBUTE: &str = "template_name";

/// the functions that resolve a route to its url by name
pub(crate) const REVERSE_CALLEES: &[&str] = &["reverse", "reverse_lazy"];

/// the function that redirects to a route named the same way
///
/// it is kept apart from [`REVERSE_CALLEES`] because it accepts a model or a url
/// path just as happily as a route name, and so cannot be read as one on sight.
pub(crate) const REDIRECT_CALLEE: &str = "redirect";

/// the functions that give a route a reversible name
const URL_CALLEES: &[&str] = &["path", "re_path", "url"];

/// the keyword those functions take the name by
const URL_NAME_KEYWORD: &str = "name";

/// the method a rest framework router routes a viewset with
const ROUTER_REGISTER_METHOD: &str = "register";

/// the keyword a registration names its generated routes by
const ROUTER_BASENAME_KEYWORD: &str = "basename";

/// the routes a router gives every registered viewset
const ROUTER_ROUTE_SUFFIXES: &[&str] = &["list", "detail"];

/// the decorator that gives one viewset method a route of its own
const ACTION_DECORATOR: &str = "action";

/// the keyword an action names its route by
const ACTION_URL_NAME_KEYWORD: &str = "url_name";

/// the attribute a viewset's fallback basename is derived from
const VIEWSET_QUERYSET_ATTRIBUTE: &str = "queryset";

/// the manager attribute a queryset reaches its model through
const MODEL_MANAGER_ATTRIBUTE: &str = "objects";

/// how deep below the project root a `templates`/`static` directory is looked for
///
/// django's own layout puts them at `<project>/<app>/templates`, and a
/// `<project>/src/<app>/templates` is about as deep as real projects go. bounding
/// the walk keeps a large repository's `node_modules` or virtualenv from being
/// crawled in full.
const DISCOVERY_DEPTH: usize = 5;

/// whether a tag or a filter was registered
#[derive(Debug, Clone, Copy, PartialEq, Eq, get_size2::GetSize)]
pub(crate) enum RegistrationKind {
    /// `@register.simple_tag` and friends. `block` is set for the
    /// `simple_block_tag` form, which takes a body and so needs an `{% end… %}`.
    Tag { block: bool },
    /// `@register.filter`
    Filter,
}

/// a tag or filter one of the project's `templatetags` modules registers
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct Registration {
    pub(crate) name: CompactString,
    pub(crate) kind: RegistrationKind,
    /// the name a `{% load %}` uses, i.e. the module's own file stem
    pub(crate) library: CompactString,
    pub(crate) file: File,
    /// the registered function's name, for navigation
    pub(crate) range: TextRange,
    pub(crate) documentation: Option<Box<str>>,
}

/// a name the project's url configuration gives a route
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct UrlName {
    /// the name a `{% url %}` reverses, namespace included
    pub(crate) name: CompactString,
    pub(crate) file: File,
    /// the `name=` literal, for navigation
    pub(crate) range: TextRange,
    /// the route pattern, shown alongside the completion
    pub(crate) route: Option<Box<str>>,
}

/// a name a view puts in a template's context
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct ContextVariable {
    pub(crate) name: CompactString,
    pub(crate) file: File,
    /// the key or attribute that declares the name, for navigation
    pub(crate) range: TextRange,
    /// the expression the view binds to the name
    ///
    /// this is what the completions infer the variable's type from, which is how
    /// a django model in the context brings its fields with it.
    pub(crate) value: Option<TextRange>,
}

/// the context one template is rendered with, from one place that renders it
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct TemplateContext {
    /// the template the view names, as the loader sees it
    pub(crate) template: CompactString,
    pub(crate) variables: Box<[ContextVariable]>,
}

/// a file inside one of the project's template directories
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct DiscoveredFile {
    /// the path relative to the directory it was found under, which is the name
    /// `{% extends %}`, `{% include %}` and `{% static %}` all use
    pub(crate) name: CompactString,
    pub(crate) path: SystemPathBuf,
}

/// every file under a `templates` directory of the project
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn template_files(db: &dyn Db, project: Project) -> Box<[DiscoveredFile]> {
    discover(db, project, TEMPLATE_DIRECTORY)
}

/// every file under a `static` directory of the project
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn static_files(db: &dyn Db, project: Project) -> Box<[DiscoveredFile]> {
    discover(db, project, STATIC_DIRECTORY)
}

/// the file a template name resolves to
pub(crate) fn resolve_template(db: &dyn Db, name: &str) -> Option<File> {
    let path = template_files(db, db.project())
        .iter()
        .find(|candidate| candidate.name == name)
        .map(|candidate| candidate.path.clone())?;

    system_path_to_file(db, &path).ok()
}

/// every tag and filter the project registers
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn registrations(db: &dyn Db, project: Project) -> Box<[Registration]> {
    project_scan(db, project, |db, file| {
        // only a module inside a `templatetags` package is a tag library, so
        // there is no point parsing anything else
        is_templatetags_module(db, file)
            .then(|| registrations_in_file(db, file).iter().cloned())
            .into_iter()
            .flatten()
    })
}

/// every name the project's url configuration defines
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn url_names(db: &dyn Db, project: Project) -> Box<[UrlName]> {
    project_scan(db, project, |db, file| {
        url_names_in_file(db, file).iter().cloned()
    })
}

/// every template context the project's views build
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn template_contexts(db: &dyn Db, project: Project) -> Box<[TemplateContext]> {
    project_scan(db, project, |db, file| {
        template_contexts_in_file(db, file).iter().cloned()
    })
}

/// the names every view that renders `template` puts in its context
///
/// two views rendering the same template contribute both their contexts, since
/// either may be the one running. a name several views agree on appears once.
pub(crate) fn context_for_template<'db>(
    db: &'db dyn Db,
    template: &str,
) -> Vec<&'db ContextVariable> {
    let mut seen: Vec<&ContextVariable> = Vec::new();

    for context in template_contexts(db, db.project())
        .iter()
        .filter(|context| context.template == template)
    {
        for variable in &context.variables {
            if !seen.iter().any(|existing| existing.name == variable.name) {
                seen.push(variable);
            }
        }
    }

    seen
}

/// run `scan` over every first-party source file of the project
fn project_scan<'db, T, I>(
    db: &'db dyn Db,
    project: Project,
    scan: impl Fn(&'db dyn Db, File) -> I,
) -> Box<[T]>
where
    I: IntoIterator<Item = T>,
{
    let mut found = Vec::new();

    for file in &project.files(db) {
        // a stub declares types, never a url pattern or a rendered template
        if file.path(db).extension() == Some("pyi") {
            continue;
        }
        found.extend(scan(db, file));
    }

    found.into_boxed_slice()
}

/// find every file under a directory named `directory`, anywhere in the project
///
/// the walk is bounded (see [`DISCOVERY_DEPTH`]) and respects the project's
/// ignore-file settings, so a repository's dependencies are not crawled.
fn discover(db: &dyn Db, project: Project, directory: &str) -> Box<[DiscoveredFile]> {
    // walking the file system is invisible to salsa, so the callers' queries would
    // hand back their first answer forever. reading the revision the project bumps
    // on every create and delete is what makes a template added mid-session show up
    let _ = project.file_system_revision(db);

    let root = project.root(db);
    let found = Mutex::new(Vec::new());

    db.system()
        .walk_directory(root)
        .standard_filters(project.settings(db).src().respect_ignore_files)
        .ignore_hidden(true)
        .run(|| {
            Box::new(|entry| {
                let Ok(entry) = entry else {
                    return WalkState::Continue;
                };

                let inside = entry
                    .path()
                    .ancestors()
                    .any(|ancestor| ancestor.file_name() == Some(directory));

                match entry.file_type() {
                    FileType::Directory => {
                        // stop descending once the entry is neither a candidate
                        // root nor already inside one
                        if !inside && entry.depth() >= DISCOVERY_DEPTH {
                            return WalkState::Skip;
                        }
                        WalkState::Continue
                    }
                    FileType::File => {
                        if inside && let Some(name) = relative_to_directory(entry.path(), directory)
                        {
                            found.lock().unwrap().push(DiscoveredFile {
                                name,
                                path: entry.path().to_path_buf(),
                            });
                        }
                        WalkState::Continue
                    }
                    FileType::Symlink => WalkState::Continue,
                }
            })
        });

    let mut found = found
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    found.sort_by(|left, right| left.name.cmp(&right.name).then(left.path.cmp(&right.path)));
    found.dedup_by(|left, right| left.name == right.name && left.path == right.path);
    found.into_boxed_slice()
}

/// the part of `path` below its innermost `directory` ancestor
///
/// `/app/templates/blog/post.html` under `templates` is `blog/post.html`, which
/// is exactly the name a `{% include %}` writes.
fn relative_to_directory(path: &SystemPath, directory: &str) -> Option<CompactString> {
    let root = path
        .ancestors()
        .find(|ancestor| ancestor.file_name() == Some(directory))?;

    Some(
        path.strip_prefix(root)
            .ok()?
            .as_str()
            .replace('\\', "/")
            .to_compact_string(),
    )
}

/// whether `file` is a module of a `templatetags` package
fn is_templatetags_module(db: &dyn Db, file: File) -> bool {
    let FilePath::System(path) = file.path(db) else {
        return false;
    };

    path.parent()
        .is_some_and(|parent| parent.file_name() == Some(TEMPLATETAGS_PACKAGE))
        && path.file_stem() != Some("__init__")
}

/// the tags and filters one `templatetags` module registers
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn registrations_in_file(db: &dyn Db, file: File) -> Box<[Registration]> {
    let Some(library) = file
        .path(db)
        .as_system_path()
        .and_then(SystemPath::file_stem)
    else {
        return Box::default();
    };
    let library = library.to_compact_string();

    let parsed = parsed_module(db, file).load(db);
    let mut visitor = RegistrationVisitor {
        file,
        library,
        registers: library_names(parsed.suite()),
        found: Vec::new(),
    };
    visitor.visit_body(parsed.suite());

    visitor.found.into_boxed_slice()
}

/// the names the module binds a `template.Library()` to
///
/// django's own documentation always calls it `register`, and code that doesn't
/// is rare enough that the conventional name is the right fallback when no
/// `Library()` call is found at all.
fn library_names(body: &[Stmt]) -> Vec<CompactString> {
    let mut names: Vec<CompactString> = body
        .iter()
        .filter_map(|statement| {
            let Stmt::Assign(assign) = statement else {
                return None;
            };
            let Expr::Call(call) = &*assign.value else {
                return None;
            };
            (callee_name(&call.func)? == "Library").then_some(&assign.targets)
        })
        .flatten()
        .filter_map(|target| match target {
            Expr::Name(name) => Some(name.id.to_compact_string()),
            _ => None,
        })
        .collect();

    if names.is_empty() {
        names.push("register".to_compact_string());
    }
    names
}

struct RegistrationVisitor {
    file: File,
    library: CompactString,
    registers: Vec<CompactString>,
    found: Vec<Registration>,
}

impl RegistrationVisitor {
    /// the registration `decorator` makes of the function named `function`
    fn registration(
        &self,
        decorator: &ast::Decorator,
        function: &ast::Identifier,
        documentation: Option<Box<str>>,
    ) -> Option<Registration> {
        // `@register.filter` and `@register.filter(name="x")` both apply
        let (attribute, call) = match &decorator.expression {
            Expr::Attribute(attribute) => (attribute, None),
            Expr::Call(call) => match &*call.func {
                Expr::Attribute(attribute) => (attribute, Some(call)),
                _ => return None,
            },
            _ => return None,
        };

        let Expr::Name(object) = &*attribute.value else {
            return None;
        };
        if !self.registers.iter().any(|name| name == object.id.as_str()) {
            return None;
        }

        let kind = match attribute.attr.as_str() {
            "filter" | "filter_function" => RegistrationKind::Filter,
            "simple_block_tag" => RegistrationKind::Tag { block: true },
            "tag" | "simple_tag" | "inclusion_tag" => RegistrationKind::Tag { block: false },
            _ => return None,
        };

        Some(Registration {
            name: registered_name(attribute.attr.as_str(), call, function),
            kind,
            library: self.library.clone(),
            file: self.file,
            range: function.range(),
            documentation,
        })
    }
}

impl<'ast> Visitor<'ast> for RegistrationVisitor {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(function) = stmt {
            let documentation = docstring_summary(&function.body);
            // one function is one tag even if it somehow carries two registering
            // decorators; the first is the one that names it
            let registration = function.decorator_list.iter().find_map(|decorator| {
                self.registration(decorator, &function.name, documentation.clone())
            });
            self.found.extend(registration);
            return;
        }

        walk_stmt(self, stmt);
    }
}

/// the name a registration goes by
///
/// `@register.inclusion_tag("card.html")` names a *template* with its first
/// argument, not the tag, which is the one case where the positional argument
/// must not be read as a name.
fn registered_name(
    attribute: &str,
    call: Option<&ast::ExprCall>,
    function: &ast::Identifier,
) -> CompactString {
    let Some(call) = call else {
        return function.id.to_compact_string();
    };

    if let Some(name) = call
        .arguments
        .find_keyword("name")
        .and_then(|keyword| string_literal(&keyword.value))
    {
        return name;
    }

    if attribute != "inclusion_tag"
        && let Some(name) = call.arguments.args.first().and_then(string_literal)
    {
        return name;
    }

    function.id.to_compact_string()
}

/// whether `file`'s source spells any of `names` somewhere
///
/// every name the scans below match is compared against an identifier that has
/// to be written out in the source, so a file whose text doesn't contain it
/// cannot produce a result and doesn't need parsing. skipping those is what
/// keeps one template's completions from parsing a project's every file.
fn mentions(db: &dyn Db, file: File, names: &[&str]) -> bool {
    let source = source_text(db, file);
    names.iter().any(|name| source.contains(name))
}

/// the url names one module defines
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn url_names_in_file(db: &dyn Db, file: File) -> Box<[UrlName]> {
    // a named route is a `path()`-like call carrying a `name=` keyword, or a
    // rest framework router registration. the viewset a registration names is
    // read through its definition rather than out of this file, so only the
    // `register` call itself has to be spelled here for the scan to find one
    let names_a_route = mentions(db, file, URL_CALLEES) && mentions(db, file, &[URL_NAME_KEYWORD]);
    if !names_a_route && !mentions(db, file, &[ROUTER_REGISTER_METHOD]) {
        return Box::default();
    }

    let parsed = parsed_module(db, file).load(db);

    // django namespaces an included module's names under its `app_name`
    let namespace = parsed.suite().iter().find_map(|statement| {
        let Stmt::Assign(assign) = statement else {
            return None;
        };
        let [Expr::Name(target)] = assign.targets.as_slice() else {
            return None;
        };
        (target.id == "app_name").then(|| string_literal(&assign.value))?
    });

    let mut visitor = UrlVisitor {
        db,
        file,
        namespace,
        found: Vec::new(),
    };
    visitor.visit_body(parsed.suite());

    visitor.found.into_boxed_slice()
}

struct UrlVisitor<'db> {
    db: &'db dyn Db,
    file: File,
    namespace: Option<CompactString>,
    found: Vec<UrlName>,
}

impl UrlVisitor<'_> {
    /// record `name`, under the module's `app_name` if it has one
    fn record(&mut self, name: &str, file: File, range: TextRange, route: Option<&str>) {
        let name = match &self.namespace {
            Some(namespace) => format!("{namespace}:{name}").to_compact_string(),
            None => name.to_compact_string(),
        };

        self.found.push(UrlName {
            name,
            file,
            range,
            route: route.map(Box::from),
        });
    }

    /// the name a `path()`-like call gives its route
    fn path_call(&mut self, call: &ast::ExprCall) {
        if !callee_name(&call.func).is_some_and(|callee| URL_CALLEES.contains(&callee.as_str())) {
            return;
        }
        let Some(keyword) = call.arguments.find_keyword(URL_NAME_KEYWORD) else {
            return;
        };
        let Some(name) = string_literal(&keyword.value) else {
            return;
        };

        let route = call.arguments.args.first().and_then(string_literal);
        self.record(&name, self.file, keyword.value.range(), route.as_deref());
    }

    /// the names a rest framework router's registration makes reversible
    ///
    /// `router.register(prefix, viewset, basename=…)` routes a whole viewset the
    /// way `path()` routes one view, and django resolves the routes it generates
    /// by name like any other. the router's class is deliberately not checked,
    /// since a project's own `SimpleRouter` subclass registers identically; what
    /// identifies a registration is the method name and the shape of its
    /// arguments, a string prefix followed by the viewset.
    fn router_registration(&mut self, call: &ast::ExprCall) {
        let Expr::Attribute(method) = &*call.func else {
            return;
        };
        if method.attr.as_str() != ROUTER_REGISTER_METHOD {
            return;
        }
        let Some(prefix) = call.arguments.find_argument_value("prefix", 0) else {
            return;
        };
        let Some(prefix) = string_literal(prefix) else {
            return;
        };
        // the viewset is named either directly or through the module it lives in
        let Some(viewset @ (Expr::Name(_) | Expr::Attribute(_))) =
            call.arguments.find_argument_value("viewset", 1)
        else {
            return;
        };

        let given = call
            .arguments
            .find_argument_value(ROUTER_BASENAME_KEYWORD, 2)
            .and_then(|given| Some((string_literal(given)?, given.range())));

        // the viewset's own file answers both what the routes are called when the
        // registration doesn't say and which actions add routes of their own. it
        // is read as best it can be: a viewset out of reach costs the actions,
        // never the names the registration already gives on its own
        let described = self.viewset(viewset);

        let (basename, anchor) = match given {
            Some(given) => given,
            // a registration whose basename can't be worked out names nothing:
            // django would reverse something, but a wrong name is worse than a
            // missing one
            None => match described
                .as_ref()
                .and_then(|described| described.basename.clone())
            {
                Some(basename) => (basename, viewset.range()),
                None => return,
            },
        };

        for suffix in ROUTER_ROUTE_SUFFIXES {
            self.record(
                &format!("{basename}-{suffix}"),
                self.file,
                anchor,
                Some(prefix.as_str()),
            );
        }

        for action in described.iter().flat_map(|described| &described.actions) {
            // an action's route is that method's, so that is where it leads
            self.record(
                &format!("{basename}-{}", action.url_name),
                action.file,
                action.range,
                Some(prefix.as_str()),
            );
        }
    }

    /// what a registered viewset says about the routes it is given
    ///
    /// following the name to its class reads another module's ast from this
    /// module's query, and that cross-file dependency is deliberate: it is what
    /// makes an `@action` added to a viewset appear in a template's completions
    /// without the url configuration being touched. it costs one file per
    /// registration, which is why it is affordable.
    fn viewset(&self, viewset: &Expr) -> Option<ViewSet> {
        let db = self.db;
        let model = SemanticModel::new(db, self.file);

        let definitions = match viewset {
            Expr::Name(name) => definitions_for_name(
                &model,
                name.id.as_str(),
                AnyNodeRef::from(name),
                ImportAliasResolution::ResolveAliases,
            ),
            Expr::Attribute(attribute) => definitions_for_attribute(&model, attribute),
            _ => return None,
        };

        definitions.into_iter().find_map(|resolved| {
            let definition = resolved.definition()?;
            let file = definition.file(db);
            let parsed = parsed_module(db, file).load(db);
            let class = definition.kind(db).as_class()?.node(&parsed);

            Some(ViewSet {
                basename: default_basename(class),
                actions: actions(class, file),
            })
        })
    }
}

/// what a viewset class contributes to the routes registering it generates
struct ViewSet {
    /// the basename a registration that doesn't give one falls back to
    basename: Option<CompactString>,
    actions: Vec<Action>,
}

/// one `@action`-decorated method of a viewset
struct Action {
    url_name: CompactString,
    file: File,
    /// the decorated method's name, for navigation
    range: TextRange,
}

/// the basename a viewset that isn't registered under one falls back to
///
/// the router takes it from `queryset.model`, lower-cased, and the model of a
/// queryset is the class its manager is reached through. a queryset built any
/// other way — from `get_queryset`, or through a manager under some other name —
/// leaves the basename unknown, and an unknown basename names no route at all.
fn default_basename(class: &ast::StmtClassDef) -> Option<CompactString> {
    let (queryset, _) = class
        .body
        .iter()
        .find_map(|statement| class_attribute(statement, VIEWSET_QUERYSET_ATTRIBUTE))?;

    Some(managed_model(queryset)?.to_lowercase().to_compact_string())
}

/// the class a manager expression hangs off, so that `models.Book.objects.all()`
/// is a `Book`
fn managed_model(expr: &Expr) -> Option<CompactString> {
    let mut current = expr;

    loop {
        match current {
            Expr::Call(call) => current = &call.func,
            Expr::Attribute(attribute) => {
                if attribute.attr.as_str() == MODEL_MANAGER_ATTRIBUTE {
                    return match &*attribute.value {
                        Expr::Name(name) => Some(name.id.to_compact_string()),
                        Expr::Attribute(owner) => Some(owner.attr.id.to_compact_string()),
                        _ => None,
                    };
                }
                current = &attribute.value;
            }
            _ => return None,
        }
    }
}

/// the extra routes a viewset's `@action` methods are given
///
/// an action that doesn't name its route takes the method's own name with its
/// underscores turned into dashes, which is what django will have to reverse.
fn actions(class: &ast::StmtClassDef, file: File) -> Vec<Action> {
    class
        .body
        .iter()
        .filter_map(|statement| {
            let Stmt::FunctionDef(function) = statement else {
                return None;
            };
            let decorator = function.decorator_list.iter().find_map(|decorator| {
                let Expr::Call(call) = &decorator.expression else {
                    return None;
                };
                (callee_name(&call.func)? == ACTION_DECORATOR).then_some(call)
            })?;

            let url_name = decorator
                .arguments
                .find_keyword(ACTION_URL_NAME_KEYWORD)
                .and_then(|keyword| string_literal(&keyword.value))
                .unwrap_or_else(|| function.name.as_str().replace('_', "-").to_compact_string());

            Some(Action {
                url_name,
                file,
                range: function.name.range(),
            })
        })
        .collect()
}

impl<'ast> Visitor<'ast> for UrlVisitor<'_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            self.path_call(call);
            self.router_registration(call);
        }

        walk_expr(self, expr);
    }
}

/// the template contexts one module builds
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn template_contexts_in_file(db: &dyn Db, file: File) -> Box<[TemplateContext]> {
    // a context comes from a render call or from a view class naming its template
    if !mentions(db, file, CONTEXT_CALLEES) && !mentions(db, file, &[TEMPLATE_NAME_ATTRIBUTE]) {
        return Box::default();
    }

    let parsed = parsed_module(db, file).load(db);

    let mut visitor = ContextVisitor {
        file,
        found: Vec::new(),
    };
    visitor.visit_body(parsed.suite());

    visitor.found.into_boxed_slice()
}

struct ContextVisitor {
    file: File,
    found: Vec<TemplateContext>,
}

impl ContextVisitor {
    /// the context a `render()`/`TemplateResponse()` call passes
    ///
    /// both take the request first, the template second and the context third,
    /// and both accept those last two by keyword as well.
    fn render_call(&self, call: &ast::ExprCall) -> Option<TemplateContext> {
        let callee = callee_name(&call.func)?;
        if !CONTEXT_CALLEES.contains(&callee.as_str()) {
            return None;
        }
        // both take the request first, the template second and the context third
        let (template_index, context_index) = (1, 2);

        let template = call
            .arguments
            .find_keyword(TEMPLATE_NAME_ATTRIBUTE)
            .map(|keyword| &keyword.value)
            .or_else(|| call.arguments.args.get(template_index))
            .and_then(string_literal)?;

        let context = call
            .arguments
            .find_keyword("context")
            .map(|keyword| &keyword.value)
            .or_else(|| call.arguments.args.get(context_index));

        Some(TemplateContext {
            template,
            variables: context
                .map(|context| self.dict_variables(context))
                .unwrap_or_default(),
        })
    }

    /// the names a dict literal binds
    fn dict_variables(&self, expr: &Expr) -> Box<[ContextVariable]> {
        let Expr::Dict(dict) = expr else {
            return Box::default();
        };

        dict.items
            .iter()
            .filter_map(|item| {
                let key = item.key.as_ref()?;
                Some(ContextVariable {
                    name: string_literal(key)?,
                    file: self.file,
                    range: key.range(),
                    value: Some(item.value.range()),
                })
            })
            .collect()
    }

    /// the context a class-based view declares
    ///
    /// django's generic views name their template with `template_name` and their
    /// object with `context_object_name`, and add anything else through
    /// `extra_context` or by writing into the dict `get_context_data` returns.
    fn class_based_view(&self, class: &ast::StmtClassDef) -> Option<TemplateContext> {
        let template = class
            .body
            .iter()
            .find_map(|statement| class_attribute(statement, TEMPLATE_NAME_ATTRIBUTE))
            .and_then(|(value, _)| string_literal(value))?;

        let mut variables = Vec::new();

        for statement in &class.body {
            if let Some((value, range)) = class_attribute(statement, "context_object_name")
                && let Some(name) = string_literal(value)
            {
                variables.push(ContextVariable {
                    name,
                    file: self.file,
                    range,
                    // the object itself is what the view will bind; its type
                    // comes from the view's generics, which is beyond what a
                    // syntactic scan can follow
                    value: None,
                });
            }

            if let Some((value, _)) = class_attribute(statement, "extra_context") {
                variables.extend(self.dict_variables(value));
            }

            if let Stmt::FunctionDef(function) = statement
                && function.name.as_str() == "get_context_data"
            {
                variables.extend(self.context_assignments(&function.body));
            }
        }

        Some(TemplateContext {
            template,
            variables: variables.into_boxed_slice(),
        })
    }

    /// the names a `get_context_data` body writes into its context dict
    fn context_assignments(&self, body: &[Stmt]) -> Vec<ContextVariable> {
        let mut found = Vec::new();

        let mut visitor = ContextAssignmentVisitor {
            file: self.file,
            found: &mut found,
        };
        visitor.visit_body(body);

        found
    }
}

impl<'ast> Visitor<'ast> for ContextVisitor {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::ClassDef(class) = stmt {
            self.found.extend(self.class_based_view(class));
            return;
        }

        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            self.found.extend(self.render_call(call));
        }

        walk_expr(self, expr);
    }
}

/// collects `context["name"] = value` writes
struct ContextAssignmentVisitor<'a> {
    file: File,
    found: &'a mut Vec<ContextVariable>,
}

impl<'ast> Visitor<'ast> for ContextAssignmentVisitor<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::Assign(assign) = stmt
            && let [Expr::Subscript(subscript)] = assign.targets.as_slice()
            && let Some(name) = string_literal(&subscript.slice)
        {
            self.found.push(ContextVariable {
                name,
                file: self.file,
                range: subscript.slice.range(),
                value: Some(assign.value.range()),
            });
        }

        walk_stmt(self, stmt);
    }
}

/// the value and name range of a class-body assignment to `attribute`
pub(crate) fn class_attribute<'ast>(
    statement: &'ast Stmt,
    attribute: &str,
) -> Option<(&'ast Expr, TextRange)> {
    match statement {
        Stmt::Assign(assign) => {
            let [Expr::Name(target)] = assign.targets.as_slice() else {
                return None;
            };
            (target.id == attribute).then(|| (&*assign.value, target.range()))
        }
        Stmt::AnnAssign(assign) => {
            let Expr::Name(target) = &*assign.target else {
                return None;
            };
            let value = assign.value.as_deref()?;
            (target.id == attribute).then(|| (value, target.range()))
        }
        _ => None,
    }
}

/// the final segment of a callee, so that `render` and `shortcuts.render` are one
pub(crate) fn callee_name(func: &Expr) -> Option<CompactString> {
    match func {
        Expr::Name(name) => Some(name.id.to_compact_string()),
        Expr::Attribute(attribute) => Some(attribute.attr.id.to_compact_string()),
        _ => None,
    }
}

/// the value of a plain string literal
fn string_literal(expr: &Expr) -> Option<CompactString> {
    match expr {
        Expr::StringLiteral(literal) => Some(literal.value.to_str().to_compact_string()),
        _ => None,
    }
}

/// the first paragraph of a body's docstring
fn docstring_summary(body: &[Stmt]) -> Option<Box<str>> {
    let Stmt::Expr(first) = body.first()? else {
        return None;
    };
    let Expr::StringLiteral(literal) = &*first.value else {
        return None;
    };

    let text = literal.value.to_str();
    let summary: String = text
        .lines()
        .map(str::trim)
        .take_while(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    (!summary.is_empty()).then(|| Box::from(summary.as_str()))
}

#[cfg(test)]
mod tests {
    use ty_project::Db;

    use crate::django_template::tests::TemplateTest;

    use super::{RegistrationKind, context_for_template, registrations, url_names};

    /// a project of python sources, with a throwaway template to anchor the
    /// harness' cursor
    fn project(sources: &[(&str, &str)]) -> TemplateTest {
        let mut all = sources.to_vec();
        all.push(("app/templates/app/page.html", "<CURSOR>"));
        TemplateTest::new(&all)
    }

    /// the names the project's url configuration defines, in the order found
    fn names(test: &TemplateTest) -> Vec<String> {
        url_names(&test.db, test.db.project())
            .iter()
            .map(|url| url.name.to_string())
            .collect()
    }

    /// the names a view puts in `template`'s context, in the order offered
    fn context(test: &TemplateTest, template: &str) -> Vec<String> {
        context_for_template(&test.db, template)
            .into_iter()
            .map(|variable| variable.name.to_string())
            .collect()
    }

    #[test]
    fn a_class_based_view_contributes_every_name_it_declares() {
        let test = project(&[(
            "app/views.py",
            "
            class BookDetail:
                template_name = 'app/detail.html'
                context_object_name = 'book'
                extra_context = {'title': 'a book'}

                def get_context_data(self, **kwargs):
                    context = super().get_context_data(**kwargs)
                    context['related'] = []
                    return context
            ",
        )]);

        assert_eq!(
            context(&test, "app/detail.html"),
            ["book", "title", "related"]
        );
    }

    #[test]
    fn a_class_based_view_without_a_template_name_names_no_template() {
        let test = project(&[(
            "app/views.py",
            "
            class Base:
                context_object_name = 'book'
            ",
        )]);

        assert!(context(&test, "app/detail.html").is_empty());
    }

    #[test]
    fn a_template_response_builds_a_context_just_as_render_does() {
        let test = project(&[(
            "app/views.py",
            "
            def show(request):
                return TemplateResponse(request, 'app/page.html', {'book': 1})
            ",
        )]);

        assert_eq!(context(&test, "app/page.html"), ["book"]);
    }

    #[test]
    fn a_render_call_may_pass_the_template_and_context_by_keyword() {
        let test = project(&[(
            "app/views.py",
            "
            def show(request):
                return render(request, template_name='app/page.html', context={'book': 1})
            ",
        )]);

        assert_eq!(context(&test, "app/page.html"), ["book"]);
    }

    #[test]
    fn two_views_of_one_template_both_contribute_and_a_shared_name_appears_once() {
        let test = project(&[(
            "app/views.py",
            "
            def one(request):
                return render(request, 'app/page.html', {'book': 1, 'shelf': 2})

            def two(request):
                return render(request, 'app/page.html', {'book': 3, 'author': 4})
            ",
        )]);

        assert_eq!(context(&test, "app/page.html"), ["book", "shelf", "author"]);
    }

    #[test]
    fn a_view_module_that_renders_nothing_is_not_parsed_for_contexts() {
        // the gate that skips it must not skip a file that does render
        let test = project(&[
            ("app/models.py", "class Book:\n    title: str\n"),
            (
                "app/views.py",
                "
                def show(request):
                    return render(request, 'app/page.html', {'book': 1})
                ",
            ),
        ]);

        assert_eq!(context(&test, "app/page.html"), ["book"]);
    }

    #[test]
    fn re_path_names_routes_too() {
        let test = project(&[(
            "app/urls.py",
            "
            urlpatterns = [
                re_path(r'^books/$', index, name='legacy'),
                path('books/', index, name='index'),
            ]
            ",
        )]);

        let names: Vec<_> = url_names(&test.db, test.db.project())
            .iter()
            .map(|url| url.name.as_str())
            .collect();
        assert_eq!(names, ["legacy", "index"]);
    }

    #[test]
    fn a_router_registration_names_a_list_and_a_detail_route() {
        let test = project(&[(
            "app/urls.py",
            "
            class BookViewSet: ...

            router = DefaultRouter()
            router.register('books', BookViewSet, basename='book')

            urlpatterns = router.urls
            ",
        )]);

        assert_eq!(names(&test), ["book-list", "book-detail"]);
    }

    #[test]
    fn an_action_that_does_not_name_its_route_takes_the_methods_own_name() {
        let test = project(&[(
            "app/urls.py",
            "
            class BookViewSet:
                @action(detail=True)
                def mark_read(self, request, pk=None): ...

            router = DefaultRouter()
            router.register('books', BookViewSet, basename='book')
            ",
        )]);

        assert_eq!(
            names(&test),
            ["book-list", "book-detail", "book-mark-read"],
            "an underscore of the method's name is a dash of the route's"
        );
    }

    #[test]
    fn an_action_may_name_its_route_itself() {
        let test = project(&[(
            "app/urls.py",
            "
            class BookViewSet:
                @action(detail=True, url_name='read')
                def mark_read(self, request, pk=None): ...

            router = DefaultRouter()
            router.register('books', BookViewSet, basename='book')
            ",
        )]);

        assert_eq!(names(&test), ["book-list", "book-detail", "book-read"]);
    }

    #[test]
    fn a_registration_without_a_basename_takes_the_viewsets_model() {
        // the viewset is a module away, as a real project's is
        let test = project(&[
            (
                "app/views.py",
                "
                from app.models import Book

                class BookViewSet:
                    queryset = Book.objects.all()
                ",
            ),
            (
                "app/urls.py",
                "
                from app.views import BookViewSet

                router = DefaultRouter()
                router.register('books', BookViewSet)
                ",
            ),
        ]);

        assert_eq!(names(&test), ["book-list", "book-detail"]);
    }

    #[test]
    fn a_viewset_may_be_named_through_the_module_it_lives_in() {
        let test = project(&[
            (
                "app/views.py",
                "
                class BookViewSet:
                    @action(detail=True)
                    def mark_read(self, request, pk=None): ...
                ",
            ),
            (
                "app/urls.py",
                "
                from app import views

                router = DefaultRouter()
                router.register('books', views.BookViewSet, basename='book')
                ",
            ),
        ]);

        assert_eq!(names(&test), ["book-list", "book-detail", "book-mark-read"]);
    }

    #[test]
    fn a_viewset_named_through_its_module_still_answers_for_its_basename() {
        let test = project(&[
            (
                "app/views.py",
                "
                class BookViewSet:
                    queryset = Book.objects.all()
                ",
            ),
            (
                "app/urls.py",
                "
                from app import views

                router = DefaultRouter()
                router.register('books', views.BookViewSet)
                ",
            ),
        ]);

        assert_eq!(names(&test), ["book-list", "book-detail"]);
    }

    #[test]
    fn a_basename_the_registration_gives_stands_without_the_viewset() {
        // only the actions need the class; the registration names the rest by
        // itself, and refusing them because a viewset is out of reach would lose
        // routes django reverses perfectly well
        let test = project(&[(
            "app/urls.py",
            "
            from third_party import views

            router = DefaultRouter()
            router.register('books', views.BookViewSet, basename='book')
            ",
        )]);

        assert_eq!(names(&test), ["book-list", "book-detail"]);
    }

    #[test]
    fn a_registration_whose_basename_cannot_be_worked_out_names_nothing() {
        let test = project(&[(
            "app/urls.py",
            "
            class BookViewSet:
                def get_queryset(self): ...

            router = DefaultRouter()
            router.register('books', BookViewSet)
            ",
        )]);

        assert!(
            names(&test).is_empty(),
            "django reverses a name here, but guessing which is worse than offering none"
        );
    }

    #[test]
    fn a_router_registrations_names_are_namespaced_like_any_others() {
        let test = project(&[(
            "app/urls.py",
            "
            app_name = 'api'

            class BookViewSet: ...

            router = DefaultRouter()
            router.register('books', BookViewSet, basename='book')
            ",
        )]);

        assert_eq!(names(&test), ["api:book-list", "api:book-detail"]);
    }

    #[test]
    fn registering_a_model_with_the_admin_is_not_a_route() {
        // it is a `register` call on an object taking a class, and the only thing
        // telling it apart from a router's is that its first argument is no prefix
        let test = project(&[(
            "app/admin.py",
            "
            class BookAdmin:
                queryset = Book.objects.all()

            admin.site.register(Book, BookAdmin)
            ",
        )]);

        assert!(names(&test).is_empty());
    }

    #[test]
    fn an_actions_name_leads_to_the_method_that_serves_it() {
        let test = project(&[
            (
                "app/views.py",
                "
                class BookViewSet:
                    @action(detail=True)
                    def mark_read(self, request, pk=None): ...
                ",
            ),
            (
                "app/urls.py",
                "
                from app.views import BookViewSet

                router = DefaultRouter()
                router.register('books', BookViewSet, basename='book')
                ",
            ),
        ]);

        let action = url_names(&test.db, test.db.project())
            .iter()
            .find(|url| url.name == "book-mark-read")
            .expect("the action's route to be named");

        assert_eq!(action.file.path(&test.db).to_string(), "/app/views.py");
        assert_eq!(
            &ruff_db::source::source_text(&test.db, action.file)[action.range],
            "mark_read"
        );
    }

    #[test]
    fn an_inclusion_tags_first_argument_is_a_template_rather_than_a_name() {
        let test = project(&[(
            "app/templatetags/app_extras.py",
            "
            from django import template

            register = template.Library()

            @register.inclusion_tag('app/card.html')
            def show_card():
                '''renders one card.'''
                return {}

            @register.simple_tag('renamed')
            def original():
                return 0
            ",
        )]);

        let found: Vec<_> = registrations(&test.db, test.db.project())
            .iter()
            .map(|registration| registration.name.as_str())
            .collect();
        assert_eq!(found, ["show_card", "renamed"]);
    }

    #[test]
    fn a_registered_function_carries_its_docstring() {
        let test = project(&[(
            "app/templatetags/app_extras.py",
            "
            from django import template

            register = template.Library()

            @register.filter
            def shout(value):
                '''upper-cases it.

                and the rest of the docstring is not the summary.
                '''
                return value
            ",
        )]);

        let documentation = registrations(&test.db, test.db.project())[0]
            .documentation
            .clone();
        assert_eq!(documentation.as_deref(), Some("upper-cases it."));
    }

    #[test]
    fn a_library_bound_to_a_name_of_its_own_still_registers() {
        let test = project(&[(
            "app/templatetags/app_extras.py",
            "
            from django import template

            library = template.Library()

            @library.filter_function
            def shout(value):
                return value
            ",
        )]);

        let found: Vec<_> = registrations(&test.db, test.db.project())
            .iter()
            .map(|registration| (registration.name.as_str(), registration.kind))
            .collect();
        assert_eq!(found, [("shout", RegistrationKind::Filter)]);
    }

    #[test]
    fn a_module_outside_a_templatetags_package_registers_nothing() {
        let test = project(&[(
            "app/extras.py",
            "
            from django import template

            register = template.Library()

            @register.filter
            def shout(value):
                return value
            ",
        )]);

        assert!(registrations(&test.db, test.db.project()).is_empty());
    }
}
