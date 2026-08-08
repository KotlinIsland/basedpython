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
use rustc_hash::{FxHashMap, FxHashSet};
use ty_module_resolver::{Module, ModuleName, resolve_module, resolve_real_module};
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

/// the environment variable a project names its settings module with
const SETTINGS_MODULE_VARIABLE: &str = "DJANGO_SETTINGS_MODULE";

/// the file stem of the script whose `DJANGO_SETTINGS_MODULE` is the one that counts
const SETTINGS_ENTRY_POINT: &str = "manage";

/// the setting that configures the template engines
const TEMPLATES_SETTING: &str = "TEMPLATES";

/// the key an engine lists its own template directories under
const DIRS_KEY: &str = "DIRS";

/// the key an engine turns the app-directories loader on with
const APP_DIRS_KEY: &str = "APP_DIRS";

/// the setting that names the project-wide static directories
const STATICFILES_DIRS_SETTING: &str = "STATICFILES_DIRS";

/// the setting that names the installed apps, in the order they are searched
const INSTALLED_APPS_SETTING: &str = "INSTALLED_APPS";

/// the key an engine passes its own options under
const OPTIONS_KEY: &str = "OPTIONS";

/// the option naming the libraries every template has loaded already
const BUILTINS_OPTION: &str = "builtins";

/// django's own package, whose `templatetags` is a library candidate like any
/// installed app's
const DJANGO_PACKAGE: &str = "django";

/// the name a module's own path is bound to
const FILE_NAME: &str = "__file__";

/// the `pathlib` attribute that walks a path upwards
const PARENT_ATTRIBUTE: &str = "parent";

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

/// the setting that names the module the project's url tree starts at
const ROOT_URLCONF_SETTING: &str = "ROOT_URLCONF";

/// the functions that give a route a reversible name
const URL_CALLEES: &[&str] = &["path", "re_path", "url"];

/// the keyword those functions take the name by
const URL_NAME_KEYWORD: &str = "name";

/// the function that mounts another url configuration under a prefix
const INCLUDE_CALLEE: &str = "include";

/// the keyword an include gives its instance namespace by
const NAMESPACE_KEYWORD: &str = "namespace";

/// the name a module namespaces the routes it holds under
const APP_NAME_VARIABLE: &str = "app_name";

/// the name a url configuration module holds its routes under
const URLPATTERNS_VARIABLE: &str = "urlpatterns";

/// the separator django writes between a namespace and the name it qualifies
const NAMESPACE_SEPARATOR: char = ':';

/// the method a rest framework router routes a viewset with
const ROUTER_REGISTER_METHOD: &str = "register";

/// the keyword a registration names its generated routes by
const ROUTER_BASENAME_KEYWORD: &str = "basename";

/// the routes a router gives every registered viewset
const ROUTER_ROUTE_SUFFIXES: &[&str] = &["list", "detail"];

/// what a class a router is built from is called, whoever wrote it
const ROUTER_CLASS_SUFFIX: &str = "Router";

/// the rest framework router that serves an index of everything registered with it
const DEFAULT_ROUTER_CLASS: &str = "DefaultRouter";

/// the rest framework routers that serve no index
const PLAIN_ROUTER_CLASSES: &[&str] = &["SimpleRouter", "BaseRouter"];

/// what a router calls that index unless it says otherwise
const ROUTER_ROOT_ROUTE: &str = "api-root";

/// the attribute a router names its index by
const ROUTER_ROOT_VIEW_ATTRIBUTE: &str = "root_view_name";

/// how many base classes deep a router's class is followed
const MAX_ROUTER_DEPTH: usize = 8;

/// how many includes deep the url tree is walked
///
/// a real project nests a handful at most, and a urlconf that includes itself
/// would nest for ever, so the walk stops here whatever it has found.
const MAX_URLCONF_DEPTH: usize = 8;

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

/// a tag or filter one of the project's tag libraries registers
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
    /// whether the library it comes from is django's own
    pub(crate) django: bool,
}

/// where a tag library comes from
#[derive(Debug, Clone, Copy, PartialEq, Eq, get_size2::GetSize)]
pub(crate) enum LibrarySource {
    /// django's own, whether shipped in `django.templatetags` or in a contrib app
    Django,
    /// one of the project's own `templatetags` modules
    Project,
    /// an installed third-party app's
    Installed,
}

/// a tag library a template can `{% load %}`
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct Library {
    /// the name a `{% load %}` names it by, i.e. the module's own file stem
    pub(crate) name: CompactString,
    /// the module the tags and filters are registered in
    pub(crate) file: File,
    pub(crate) source: LibrarySource,
    /// whether every template has it loaded already, without saying `{% load %}`
    ///
    /// `TEMPLATES[*]["OPTIONS"]["builtins"]` is how a project asks for that.
    pub(crate) always_loaded: bool,
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
    /// how early django would reach the directory this was found under
    ///
    /// two apps may both hold a `base.html`, and only one of them is the one
    /// django loads. every consumer of a discovery takes the first file of a
    /// name, so this is what puts that one first.
    precedence: usize,
}

/// every file under a `templates` directory of the project, or one its settings name
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn template_files(db: &dyn Db, project: Project) -> Box<[DiscoveredFile]> {
    discover(
        db,
        project,
        TEMPLATE_DIRECTORY,
        template_search_order(db, project),
    )
}

/// every file under a `static` directory of the project, or one its settings name
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn static_files(db: &dyn Db, project: Project) -> Box<[DiscoveredFile]> {
    discover(
        db,
        project,
        STATIC_DIRECTORY,
        static_search_order(db, project),
    )
}

/// the file a template name resolves to
pub(crate) fn resolve_template(db: &dyn Db, name: &str) -> Option<File> {
    let path = template_files(db, db.project())
        .iter()
        .find(|candidate| candidate.name == name)
        .map(|candidate| candidate.path.clone())?;

    system_path_to_file(db, &path).ok()
}

/// every tag and filter the project can use
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn registrations(db: &dyn Db, project: Project) -> Box<[Registration]> {
    tag_libraries(db, project)
        .iter()
        .flat_map(|library| {
            registrations_in_file(db, library.file)
                .iter()
                .map(|registration| Registration {
                    django: library.source == LibrarySource::Django,
                    ..registration.clone()
                })
        })
        .collect()
}

/// every tag library the project can `{% load %}`
///
/// the project's own `templatetags` modules are one source; the other is the
/// installed apps, which is where django's own contrib libraries and any
/// third-party app's come from. a project whose settings can't be read has only
/// the first, and behaves as though the second had found nothing.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn tag_libraries(db: &dyn Db, project: Project) -> Box<[Library]> {
    let mut found: Vec<Library> = Vec::new();

    // the project's own, wherever in it they sit. these come first so that an app
    // that is both the project's and installed is reported as the project's
    for file in &project.files(db) {
        if !is_stub(db, file) && is_templatetags_module(db, file) {
            found.extend(library(db, file, LibrarySource::Project, false));
        }
    }

    for installed in installed_libraries(db, project) {
        merge(&mut found, installed.clone());
    }

    found.into_boxed_slice()
}

/// add `discovered` to `found`, or fold it into the one already there
///
/// one module is one library however many ways it was reached. the way it was
/// first reached is the one that says where it came from — an app of the
/// project's own stays the project's however it is installed — but any of them
/// saying every template has it loaded already settles that for all of them.
fn merge(found: &mut Vec<Library>, discovered: Library) {
    if let Some(existing) = found
        .iter_mut()
        .find(|existing| existing.file == discovered.file)
    {
        existing.always_loaded |= discovered.always_loaded;
        return;
    }

    found.push(discovered);
}

/// the library `file` is, named the way a `{% load %}` names it
fn library(db: &dyn Db, file: File, source: LibrarySource, always_loaded: bool) -> Option<Library> {
    let name = file
        .path(db)
        .as_system_path()
        .and_then(SystemPath::file_stem)?;

    Some(Library {
        name: name.to_compact_string(),
        file,
        source,
        always_loaded,
    })
}

/// the tag libraries the project's installed apps bring with them
///
/// this is what django's own `get_installed_libraries` walks: the `templatetags`
/// subpackage of each installed app, and django's own `django.templatetags`
/// alongside them. site-packages at large is never searched — an app that isn't
/// installed contributes nothing, exactly as at render time.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn installed_libraries(db: &dyn Db, project: Project) -> Box<[Library]> {
    let Some(importing) = *settings_file(db, project) else {
        return Box::default();
    };
    let settings = django_settings(db, project);

    let mut found: Vec<Library> = Vec::new();

    for app in std::iter::once(DJANGO_PACKAGE)
        .chain(settings.installed_apps.iter().map(CompactString::as_str))
    {
        let Some(package) = app_package(db, importing, app) else {
            continue;
        };
        let Some(tags) = templatetags_package(db, importing, package) else {
            continue;
        };
        let source = if is_djangos(db, package) {
            LibrarySource::Django
        } else {
            LibrarySource::Installed
        };

        for module in tags.all_submodules(db) {
            // a stub carries no `@register.filter` to find, so it is the runtime
            // module beside it that is scanned — and never both
            let Some(file) = module.file(db).filter(|file| !is_stub(db, *file)) else {
                continue;
            };
            if let Some(discovered) = library(db, file, source, false) {
                merge(&mut found, discovered);
            }
        }
    }

    // a library the settings load into every template is available whether or not
    // any app installs it, and needs no `{% load %}` wherever it came from
    for path in &settings.always_loaded {
        let Some(name) = ModuleName::new(path) else {
            continue;
        };
        let Some(file) =
            resolve_real_module(db, importing, &name).and_then(|module| module.file(db))
        else {
            continue;
        };
        let source = if name.components().next() == Some(DJANGO_PACKAGE) {
            LibrarySource::Django
        } else {
            LibrarySource::Installed
        };

        if let Some(discovered) = library(db, file, source, true) {
            merge(&mut found, discovered);
        }
    }

    found.into_boxed_slice()
}

/// the package an `INSTALLED_APPS` entry names
///
/// an entry names either the app's package (`"blog"`) or an `AppConfig` inside it
/// (`"blog.apps.BlogConfig"`), and django takes the app to be the package the
/// config lives in — which is the innermost package the entry names. it is the
/// runtime module that is wanted rather than a stub beside it, since what a
/// library registers is only ever written in the module that runs.
fn app_package<'db>(db: &'db dyn Db, importing: File, app: &str) -> Option<Module<'db>> {
    let segments: Vec<&str> = app.split('.').collect();

    (1..=segments.len()).rev().find_map(|length| {
        let name = ModuleName::from_components(segments[..length].iter().copied())?;
        let module = resolve_real_module(db, importing, &name)?;

        module.kind(db).is_package().then_some(module)
    })
}

/// the `templatetags` package of an app, when it has one
fn templatetags_package<'db>(
    db: &'db dyn Db,
    importing: File,
    package: Module<'db>,
) -> Option<Module<'db>> {
    let mut name = package.name(db).clone();
    name.extend(&ModuleName::new(TEMPLATETAGS_PACKAGE)?);

    resolve_real_module(db, importing, &name)
}

/// whether a module is one of django's own
fn is_djangos(db: &dyn Db, module: Module<'_>) -> bool {
    module.name(db).components().next() == Some(DJANGO_PACKAGE)
}

/// whether `file` is a stub, which declares types but registers and renders nothing
fn is_stub(db: &dyn Db, file: File) -> bool {
    matches!(file.path(db).extension(), Some("pyi" | "byi"))
}

/// every name the project's url configuration defines
///
/// a project that says where its url tree starts has that tree walked from
/// there, which is the only way to know which namespace an include puts the
/// names it mounts under — and the only way to reach a third-party urlconf,
/// which is no file of the project's own.
///
/// a project that says nothing is scanned flat instead: every module of it that
/// names a route contributes, under its own `app_name` where it has one. that
/// misses an include's namespace and reaches nothing installed, but it is what a
/// project whose settings can't be read has, and having more names than django
/// costs a suggestion where having fewer would cost a wrong diagnostic.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn url_names(db: &dyn Db, project: Project) -> Box<[UrlName]> {
    let Some(root) = *root_urlconf(db, project) else {
        return project_scan(db, project, flat_url_names);
    };

    let mut walk = UrlWalk {
        db,
        found: Vec::new(),
        visited: FxHashSet::default(),
    };
    walk.mount(root, URLPATTERNS_VARIABLE, "", MAX_URLCONF_DEPTH);

    let mut found = walk.found;
    let mut seen = FxHashSet::default();
    found.retain(|url| seen.insert((url.name.clone(), url.file, url.range)));

    found.into_boxed_slice()
}

/// the module the project's url tree starts at
///
/// it is a dotted module name like any other, so an installed package may be the
/// one that holds it.
#[salsa::tracked]
fn root_urlconf(db: &dyn Db, project: Project) -> Option<File> {
    let importing = (*settings_file(db, project))?;
    let root = django_settings(db, project).root_urlconf.as_ref()?;

    resolve_real_module(db, importing, &ModuleName::new(root)?)?.file(db)
}

/// the names a module defines read on its own, the way the flat scan reads them
fn flat_url_names(db: &dyn Db, file: File) -> impl Iterator<Item = UrlName> {
    let conf = urlconf(db, file);
    let namespace = conf.app_name.clone().unwrap_or_default();

    conf.entries
        .iter()
        .filter_map(move |entry| match &entry.kind {
            UrlEntryKind::Route(route) => Some(namespaced(route, &namespace)),
            // an include is a tree the flat scan has no way to walk: the module it
            // names contributes its own names when the scan reaches it
            UrlEntryKind::Include(_) => None,
        })
}

/// walks the url tree, applying the namespace of every include it goes through
struct UrlWalk<'db> {
    db: &'db dyn Db,
    found: Vec<UrlName>,
    /// the module, list and namespace triples already walked
    ///
    /// a urlconf may include itself, directly or around a loop, and two includes
    /// of one module under two namespaces are two different sets of names — so
    /// it is the whole triple that decides whether there is anything left to do.
    visited: FxHashSet<(File, CompactString, CompactString)>,
}

impl UrlWalk<'_> {
    /// walk the routes `binding` of `file` holds, namespaced under `prefix`
    fn mount(&mut self, file: File, binding: &str, prefix: &str, depth: usize) {
        if depth == 0
            || !self.visited.insert((
                file,
                binding.to_compact_string(),
                prefix.to_compact_string(),
            ))
        {
            return;
        }

        let db = self.db;
        // including a module mounts its `urlpatterns`, and a route the module
        // writes outside any list at all is taken along rather than lost
        let whole_module = binding == URLPATTERNS_VARIABLE;

        for entry in &urlconf(db, file).entries {
            let mounted = match &entry.binding {
                Some(bound) => bound == binding,
                None => whole_module,
            };
            if !mounted {
                continue;
            }

            match &entry.kind {
                UrlEntryKind::Route(route) => self.found.push(namespaced(route, prefix)),
                UrlEntryKind::Include(include) => self.follow(file, include, prefix, depth),
            }
        }
    }

    /// walk what an include mounts, under the namespace it mounts it in
    fn follow(&mut self, file: File, include: &Include, prefix: &str, depth: usize) {
        match &include.target {
            IncludeTarget::Local(binding) => {
                let prefix = extend(prefix, include.namespace.as_deref());
                self.mount(file, binding, &prefix, depth - 1);
            }
            IncludeTarget::Module(module) => {
                let Some(included) = ModuleName::new(module)
                    .and_then(|module| resolve_real_module(self.db, file, &module))
                    .and_then(|module| module.file(self.db))
                else {
                    return;
                };
                // an include that names no namespace leaves the included module
                // to name its own
                let namespace = include
                    .namespace
                    .clone()
                    .or_else(|| urlconf(self.db, included).app_name.clone());
                let prefix = extend(prefix, namespace.as_deref());

                self.mount(included, URLPATTERNS_VARIABLE, &prefix, depth - 1);
            }
        }
    }
}

/// `prefix` with `namespace` qualified under it, as django writes it
fn extend(prefix: &str, namespace: Option<&str>) -> CompactString {
    match namespace {
        Some(namespace) if prefix.is_empty() => namespace.to_compact_string(),
        Some(namespace) => format!("{prefix}{NAMESPACE_SEPARATOR}{namespace}").to_compact_string(),
        None => prefix.to_compact_string(),
    }
}

/// `route` as it is reversed from under `prefix`
fn namespaced(route: &UrlName, prefix: &str) -> UrlName {
    UrlName {
        name: extend(prefix, Some(&route.name)),
        ..route.clone()
    }
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
        if is_stub(db, file) {
            continue;
        }
        found.extend(scan(db, file));
    }

    found.into_boxed_slice()
}

/// find every file django could load, under the convention and under `order`
///
/// the two are a union rather than a choice. a settings module that can't be
/// found, or a directory in one that can't be worked out, must leave the
/// convention-based discovery exactly as it was — under-reporting a template is
/// what makes a "no such template" wrong, and over-reporting one only costs a
/// suggestion nobody asked for.
fn discover(
    db: &dyn Db,
    project: Project,
    directory: &str,
    order: &SearchOrder,
) -> Box<[DiscoveredFile]> {
    // walking the file system is invisible to salsa, so the callers' queries would
    // hand back their first answer forever. reading the revision the project bumps
    // on every create and delete is what makes a template added mid-session show up
    let _ = project.file_system_revision(db);

    let mut found = discover_by_convention(db, project, directory, order);

    // a directory the settings name is under no obligation to be called
    // `templates`, nor to sit under the project root, so it gets a walk of its own
    for (precedence, root) in order.named.iter().enumerate() {
        collect_under(db, project, root, precedence, &mut found);
    }

    found.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.precedence.cmp(&right.precedence))
            .then(left.path.cmp(&right.path))
    });
    found.dedup_by(|left, right| left.name == right.name && left.path == right.path);
    found.into_boxed_slice()
}

/// find every file under a directory named `directory`, anywhere in the project
///
/// the walk is bounded (see [`DISCOVERY_DEPTH`]) and respects the project's
/// ignore-file settings, so a repository's dependencies are not crawled.
fn discover_by_convention(
    db: &dyn Db,
    project: Project,
    directory: &str,
    order: &SearchOrder,
) -> Vec<DiscoveredFile> {
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
                        if inside
                            && let Some((under, name)) =
                                relative_to_directory(entry.path(), directory)
                        {
                            found.lock().unwrap().push(DiscoveredFile {
                                name,
                                path: entry.path().to_path_buf(),
                                precedence: order.rank(under),
                            });
                        }
                        WalkState::Continue
                    }
                    FileType::Symlink => WalkState::Continue,
                }
            })
        });

    found
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// add every file under `root` to `found`, named the way django would name it
fn collect_under(
    db: &dyn Db,
    project: Project,
    root: &SystemPath,
    precedence: usize,
    found: &mut Vec<DiscoveredFile>,
) {
    let collected = Mutex::new(Vec::new());

    db.system()
        .walk_directory(root)
        .standard_filters(project.settings(db).src().respect_ignore_files)
        .ignore_hidden(true)
        .run(|| {
            Box::new(|entry| {
                let Ok(entry) = entry else {
                    return WalkState::Continue;
                };

                if matches!(entry.file_type(), FileType::File)
                    && let Some(name) = relative_to_root(entry.path(), root)
                {
                    collected.lock().unwrap().push(DiscoveredFile {
                        name,
                        path: entry.path().to_path_buf(),
                        precedence,
                    });
                }

                WalkState::Continue
            })
        });

    found.extend(
        collected
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
}

/// the innermost `directory` ancestor of `path`, and the part of `path` below it
///
/// `/app/templates/blog/post.html` under `templates` is `blog/post.html`, which
/// is exactly the name a `{% include %}` writes.
fn relative_to_directory<'a>(
    path: &'a SystemPath,
    directory: &str,
) -> Option<(&'a SystemPath, CompactString)> {
    let root = path
        .ancestors()
        .find(|ancestor| ancestor.file_name() == Some(directory))?;

    Some((root, relative_to_root(path, root)?))
}

/// the part of `path` below `root`, as django's loader would name it
fn relative_to_root(path: &SystemPath, root: &SystemPath) -> Option<CompactString> {
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
            // one module is one library however it was reached, so where it came
            // from is [`registrations`]' to say rather than this scan's
            django: false,
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

/// where django would look for a kind of file, in the order it looks
///
/// django tries the directories the settings name outright before it tries the
/// installed apps' own, and the apps in the order `INSTALLED_APPS` lists them.
/// that order is what decides which of two same-named templates is loaded.
#[derive(Debug, Default, Clone, PartialEq, Eq, get_size2::GetSize)]
struct SearchOrder {
    /// the directories the settings name outright
    named: Box<[SystemPathBuf]>,
    /// the installed apps' own directories, in the order they are searched
    apps: Box<[SystemPathBuf]>,
}

impl SearchOrder {
    /// how early django would reach `root`, later than anything known if never
    ///
    /// a directory the settings say nothing about is still discovered — the
    /// convention finds it — but nothing says which of two such directories
    /// django prefers, so they keep their existing order among themselves.
    fn rank(&self, root: &SystemPath) -> usize {
        if let Some(index) = self.named.iter().position(|named| **named == *root) {
            return index;
        }

        // an app's templates sit one directory below the app itself
        if let Some(parent) = root.parent()
            && let Some(index) = self.apps.iter().position(|app| **app == *parent)
        {
            return self.named.len() + index;
        }

        usize::MAX
    }
}

/// where django would look for the project's templates
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn template_search_order(db: &dyn Db, project: Project) -> SearchOrder {
    let settings = django_settings(db, project);

    SearchOrder {
        named: settings.template_dirs.clone(),
        // the app-directories loader is the only thing that searches the
        // installed apps, so with it off there is no order to speak of. the
        // directories are still discovered either way
        apps: if settings.app_directories {
            app_directories(db, project, &settings.installed_apps)
        } else {
            Box::default()
        },
    }
}

/// where django would look for the project's static files
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn static_search_order(db: &dyn Db, project: Project) -> SearchOrder {
    let settings = django_settings(db, project);

    SearchOrder {
        named: settings.static_dirs.clone(),
        // `staticfiles` searches the installed apps whatever the template
        // engines are configured to do
        apps: app_directories(db, project, &settings.installed_apps),
    }
}

/// the directory each of `apps` lives in, in the order given
///
/// an app that can't be resolved contributes no directory rather than a guessed one.
fn app_directories(db: &dyn Db, project: Project, apps: &[CompactString]) -> Box<[SystemPathBuf]> {
    let Some(importing) = *settings_file(db, project) else {
        return Box::default();
    };

    apps.iter()
        .filter_map(|app| {
            app_package(db, importing, app)?
                .file(db)?
                .path(db)
                .as_system_path()?
                .parent()
                .map(SystemPath::to_path_buf)
        })
        .collect()
}

/// what the project's django settings module says about where its files live
#[derive(Debug, Default, Clone, PartialEq, Eq, get_size2::GetSize)]
struct DjangoSettings {
    /// the directories `TEMPLATES[*]["DIRS"]` names
    template_dirs: Box<[SystemPathBuf]>,
    /// whether any template engine has `APP_DIRS` on
    app_directories: bool,
    /// the directories `STATICFILES_DIRS` names
    static_dirs: Box<[SystemPathBuf]>,
    /// the apps `INSTALLED_APPS` names, in the order it names them
    installed_apps: Box<[CompactString]>,
    /// the module `ROOT_URLCONF` names, where the project's url tree starts
    root_urlconf: Option<CompactString>,
    /// the modules `TEMPLATES[*]["OPTIONS"]["builtins"]` names, whose tags and
    /// filters every template has without saying `{% load %}`
    always_loaded: Box<[CompactString]>,
}

/// what the project's settings module says
///
/// only the module `DJANGO_SETTINGS_MODULE` names is read. a settings module
/// ending in `from .local import *` is a common enough shape, but following it
/// would mean deciding which of two disagreeing values django ends up with, so
/// it is not followed and the named module's own answer stands.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn django_settings(db: &dyn Db, project: Project) -> DjangoSettings {
    let Some(file) = *settings_file(db, project) else {
        return DjangoSettings::default();
    };
    let Some(path) = file.path(db).as_system_path() else {
        return DjangoSettings::default();
    };

    let parsed = parsed_module(db, file).load(db);

    let mut reader = SettingsReader {
        paths: PathEvaluator {
            file: path.to_path_buf(),
            root: project.root(db).to_path_buf(),
            names: FxHashMap::default(),
        },
        settings: DjangoSettings::default(),
    };
    reader.read(parsed.suite());

    reader.settings
}

/// the settings module the project points `DJANGO_SETTINGS_MODULE` at
///
/// a project names it in `manage.py`, and usually again in its `wsgi.py` and
/// `asgi.py`. where they disagree it is `manage.py` that decides, since that is
/// the one a developer runs.
#[salsa::tracked]
fn settings_file(db: &dyn Db, project: Project) -> Option<File> {
    let mut naming: Vec<(File, CompactString)> = Vec::new();

    for file in &project.files(db) {
        // a stub sets no environment variable
        if is_stub(db, file) {
            continue;
        }
        if let Some(module) = settings_module_in_file(db, file) {
            naming.push((file, module.clone()));
        }
    }

    // the project's files come in no order worth relying on, and the fallback has
    // to land on the same file twice running
    naming.sort_by(|(left, _), (right, _)| left.path(db).as_str().cmp(right.path(db).as_str()));

    let (importing, module) = naming
        .iter()
        .find(|(file, _)| {
            file.path(db)
                .as_system_path()
                .and_then(SystemPath::file_stem)
                == Some(SETTINGS_ENTRY_POINT)
        })
        .or_else(|| naming.first())?;

    resolve_module(db, *importing, &ModuleName::new(module)?)?.file(db)
}

/// the settings module `file` points `DJANGO_SETTINGS_MODULE` at
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn settings_module_in_file(db: &dyn Db, file: File) -> Option<CompactString> {
    if !mentions(db, file, &[SETTINGS_MODULE_VARIABLE]) {
        return None;
    }

    let parsed = parsed_module(db, file).load(db);
    let mut visitor = SettingsModuleVisitor { found: None };
    visitor.visit_body(parsed.suite());

    visitor.found
}

/// finds the one string that names the settings module
struct SettingsModuleVisitor {
    found: Option<CompactString>,
}

impl<'ast> Visitor<'ast> for SettingsModuleVisitor {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // `os.environ["DJANGO_SETTINGS_MODULE"] = "project.settings"`
        if self.found.is_none()
            && let Stmt::Assign(assign) = stmt
            && let [Expr::Subscript(subscript)] = assign.targets.as_slice()
            && string_literal(&subscript.slice).as_deref() == Some(SETTINGS_MODULE_VARIABLE)
        {
            self.found = string_literal(&assign.value);
            return;
        }

        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        // `os.environ.setdefault("DJANGO_SETTINGS_MODULE", "project.settings")`,
        // and every other two-string call that sets the variable the same way
        if self.found.is_none()
            && let Expr::Call(call) = expr
            && let [variable, module] = call.arguments.args.as_ref()
            && string_literal(variable).as_deref() == Some(SETTINGS_MODULE_VARIABLE)
        {
            self.found = string_literal(module);
            return;
        }

        walk_expr(self, expr);
    }
}

/// reads a settings module's assignments into the settings that matter here
struct SettingsReader {
    paths: PathEvaluator,
    settings: DjangoSettings,
}

impl SettingsReader {
    /// read every setting the module assigns at its top level
    ///
    /// a setting written any other way — built up by a helper, mutated after the
    /// fact as `TEMPLATES[0]["DIRS"] += …` — is not read, and contributes
    /// nothing rather than something wrong.
    fn read(&mut self, body: &[Stmt]) {
        let mut template_dirs = Vec::new();
        let mut static_dirs = Vec::new();
        let mut installed_apps = Vec::new();

        for statement in body {
            let Stmt::Assign(assign) = statement else {
                continue;
            };
            let [Expr::Name(target)] = assign.targets.as_slice() else {
                continue;
            };

            match target.id.as_str() {
                TEMPLATES_SETTING => self.engines(&assign.value, &mut template_dirs),
                STATICFILES_DIRS_SETTING => static_dirs.extend(self.directories(&assign.value)),
                INSTALLED_APPS_SETTING => {
                    installed_apps
                        .extend(elements(&assign.value).iter().filter_map(string_literal));
                }
                ROOT_URLCONF_SETTING => {
                    self.settings.root_urlconf = string_literal(&assign.value);
                }
                // any other name may be the `BASE_DIR` the directories above are
                // written against, so it is worked out in case one of them is
                _ => {
                    if let Some(path) = self.paths.path(&assign.value) {
                        self.paths.names.insert(target.id.to_compact_string(), path);
                    }
                }
            }
        }

        self.settings.template_dirs = template_dirs.into_boxed_slice();
        self.settings.static_dirs = static_dirs.into_boxed_slice();
        self.settings.installed_apps = installed_apps.into_boxed_slice();
    }

    /// read the `DIRS` and `APP_DIRS` of every configured template engine
    fn engines(&mut self, expr: &Expr, dirs: &mut Vec<SystemPathBuf>) {
        for engine in elements(expr) {
            let Expr::Dict(engine) = engine else {
                continue;
            };

            for item in &engine.items {
                let Some(key) = item.key.as_ref().and_then(string_literal) else {
                    continue;
                };

                match key.as_str() {
                    DIRS_KEY => dirs.extend(self.directories(&item.value)),
                    APP_DIRS_KEY => {
                        if matches!(&item.value, Expr::BooleanLiteral(literal) if literal.value) {
                            self.settings.app_directories = true;
                        }
                    }
                    OPTIONS_KEY => self.options(&item.value),
                    _ => {}
                }
            }
        }
    }

    /// read the engine options that matter here
    ///
    /// `builtins` is the one: a module it names is loaded into every template the
    /// engine renders, so nothing in a template has to `{% load %}` it.
    fn options(&mut self, expr: &Expr) {
        let Expr::Dict(options) = expr else {
            return;
        };

        let mut always_loaded: Vec<CompactString> = self.settings.always_loaded.to_vec();

        for item in &options.items {
            if item.key.as_ref().and_then(string_literal).as_deref() == Some(BUILTINS_OPTION) {
                always_loaded.extend(elements(&item.value).iter().filter_map(string_literal));
            }
        }

        self.settings.always_loaded = always_loaded.into_boxed_slice();
    }

    /// the directories a list of them names, the ones that can't be worked out dropped
    fn directories(&self, expr: &Expr) -> Vec<SystemPathBuf> {
        elements(expr)
            .iter()
            .filter_map(|element| self.paths.path(element))
            .collect()
    }
}

/// the elements of a list or tuple literal, and nothing for anything else
fn elements(expr: &Expr) -> &[Expr] {
    match expr {
        Expr::List(list) => &list.elts,
        Expr::Tuple(tuple) => &tuple.elts,
        _ => &[],
    }
}

/// works out which directory a settings expression names
///
/// settings write their directories as often as not through a `BASE_DIR`, itself
/// a `pathlib` or `os.path` expression over `__file__`. only what reduces to a
/// directory is answered: an environment lookup, a call into the project's own
/// code, or a name assigned somewhere other than the module's top level all stay
/// unresolved, and an unresolved directory is one django is not told about here.
struct PathEvaluator {
    /// the settings module's own path, which is what `__file__` is
    file: SystemPathBuf,
    /// what a relative directory is taken against
    root: SystemPathBuf,
    /// the module-level names already worked out, `BASE_DIR` chief among them
    names: FxHashMap<CompactString, SystemPathBuf>,
}

impl PathEvaluator {
    fn path(&self, expr: &Expr) -> Option<SystemPathBuf> {
        match expr {
            Expr::StringLiteral(literal) => Some(self.root.join(literal.value.to_str())),
            Expr::Name(name) if name.id == FILE_NAME => Some(self.file.clone()),
            Expr::Name(name) => self.names.get(name.id.as_str()).cloned(),
            // `BASE_DIR / "templates"`
            Expr::BinOp(binary) if binary.op == ast::Operator::Div => {
                let mut path = self.path(&binary.left)?;
                path.push(string_literal(&binary.right)?.as_str());
                Some(path)
            }
            Expr::Attribute(attribute) if attribute.attr.as_str() == PARENT_ATTRIBUTE => {
                Some(self.path(&attribute.value)?.parent()?.to_path_buf())
            }
            Expr::Call(call) => self.call(call),
            _ => None,
        }
    }

    /// the directory a call names, for the calls that only ever rewrite one
    fn call(&self, call: &ast::ExprCall) -> Option<SystemPathBuf> {
        let callee = callee_name(&call.func)?;
        let first = call.arguments.args.first();

        match callee.as_str() {
            // wrappers that hand back the path they are given
            "Path" | "PurePath" | "PosixPath" | "str" | "abspath" | "realpath" | "normpath" => {
                self.path(first?)
            }
            // the same, written as a method of the path itself
            "resolve" | "absolute" | "expanduser" => match &*call.func {
                Expr::Attribute(attribute) => self.path(&attribute.value),
                _ => None,
            },
            "dirname" => Some(self.path(first?)?.parent()?.to_path_buf()),
            // `os.path.join(BASE_DIR, "templates")`
            "join" => {
                let mut arguments = call.arguments.args.iter();
                let mut path = self.path(arguments.next()?)?;

                for argument in arguments {
                    path.push(string_literal(argument)?.as_str());
                }
                Some(path)
            }
            _ => None,
        }
    }
}

/// what one url configuration module contributes
///
/// the entries are read without a namespace applied: which namespace a name ends
/// up under is for the include that reaches it to say, and one module reached
/// two ways is namespaced two ways.
#[derive(Debug, Default, Clone, PartialEq, Eq, get_size2::GetSize)]
struct UrlConf {
    /// the namespace an include of this module falls back to
    app_name: Option<CompactString>,
    entries: Box<[UrlEntry]>,
}

/// one thing a url configuration module does
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
struct UrlEntry {
    /// the module-level name it is bound under, `urlpatterns` most often
    ///
    /// this is what tells an `include(router.urls)` which of the module's lists
    /// it is mounting, and what keeps a list nothing mounts out of the answer.
    binding: Option<CompactString>,
    kind: UrlEntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
enum UrlEntryKind {
    /// a route the module names itself
    Route(UrlName),
    /// a list of routes mounted under a namespace of its own
    Include(Include),
}

/// another list of routes, and the namespace the including site gives it
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
struct Include {
    target: IncludeTarget,
    /// the `namespace=`, or the instance namespace the two-tuple form names
    namespace: Option<CompactString>,
}

/// what an include mounts
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
enum IncludeTarget {
    /// a urlconf named by its dotted module path, the project's or an installed one
    Module(CompactString),
    /// a list of routes bound in the same module
    Local(CompactString),
}

/// the url configuration one module holds
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn urlconf(db: &dyn Db, file: File) -> UrlConf {
    // a named route is a `path()`-like call carrying a `name=` keyword, or a
    // rest framework router registration. the viewset a registration names is
    // read through its definition rather than out of this file, so only the
    // `register` call itself has to be spelled here for the scan to find one —
    // and a module that only mounts other modules, or only builds a router,
    // spells the call that does it
    let names_a_route = mentions(db, file, URL_CALLEES) && mentions(db, file, &[URL_NAME_KEYWORD]);
    if !names_a_route
        && !mentions(
            db,
            file,
            &[ROUTER_REGISTER_METHOD, INCLUDE_CALLEE, ROUTER_CLASS_SUFFIX],
        )
    {
        return UrlConf::default();
    }

    let parsed = parsed_module(db, file).load(db);

    // django namespaces an included module's names under its `app_name` unless
    // the include says otherwise
    let app_name = parsed.suite().iter().find_map(|statement| {
        let (target, value) = assignment(statement)?;
        (target == APP_NAME_VARIABLE).then(|| string_literal(value))?
    });

    let mut visitor = UrlVisitor {
        db,
        file,
        binding: None,
        found: Vec::new(),
    };
    visitor.visit_body(parsed.suite());

    UrlConf {
        app_name,
        entries: visitor.found.into_boxed_slice(),
    }
}

struct UrlVisitor<'db> {
    db: &'db dyn Db,
    file: File,
    /// the module-level name the statement being visited binds
    binding: Option<CompactString>,
    found: Vec<UrlEntry>,
}

impl UrlVisitor<'_> {
    /// record `name` as a route of the list `binding`
    fn record(
        &mut self,
        binding: Option<CompactString>,
        name: &str,
        file: File,
        range: TextRange,
        route: Option<&str>,
    ) {
        self.found.push(UrlEntry {
            binding,
            kind: UrlEntryKind::Route(UrlName {
                name: name.to_compact_string(),
                file,
                range,
                route: route.map(Box::from),
            }),
        });
    }

    /// record that the list being bound mounts `target` under `namespace`
    fn mounts(&mut self, target: IncludeTarget, namespace: Option<CompactString>) {
        self.found.push(UrlEntry {
            binding: self.binding.clone(),
            kind: UrlEntryKind::Include(Include { target, namespace }),
        });
    }

    /// the url configuration an `include()` mounts
    ///
    /// `include("blog.urls")` mounts another module, `include(router.urls)` a
    /// list of this one's, and either may be written as a `(target, namespace)`
    /// pair. an explicit `namespace=` outranks the pair's, and both outrank the
    /// included module's own `app_name`.
    fn include_call(&mut self, call: &ast::ExprCall) {
        if callee_name(&call.func).as_deref() != Some(INCLUDE_CALLEE) {
            return;
        }
        let Some(argument) = call.arguments.args.first() else {
            return;
        };

        let (target, instance) = match argument {
            Expr::Tuple(pair) => match pair.elts.as_slice() {
                [target, instance] => (target, string_literal(instance)),
                _ => return,
            },
            _ => (argument, None),
        };
        let Some(target) = include_target(target) else {
            return;
        };

        let namespace = call
            .arguments
            .find_keyword(NAMESPACE_KEYWORD)
            .and_then(|keyword| string_literal(&keyword.value))
            .or(instance);

        self.mounts(target, namespace);
    }

    /// the index route a router built by `call` serves of its own
    ///
    /// a `DefaultRouter` serves one and names it like any other route; a
    /// `SimpleRouter` serves none. a router class that can't be told apart is
    /// taken to serve one, since a name offered that django hasn't got costs a
    /// suggestion where a name missing would cost a wrong diagnostic.
    fn router_root(&mut self, binding: &str, call: &ast::ExprCall) {
        if !callee_name(&call.func).is_some_and(|class| class.ends_with(ROUTER_CLASS_SUFFIX)) {
            return;
        }

        let name = match router_root_route(self.db, self.file, &call.func, MAX_ROUTER_DEPTH) {
            RouterRoot::Named(name) => name,
            RouterRoot::None => return,
            RouterRoot::Unknown => ROUTER_ROOT_ROUTE.to_compact_string(),
        };

        self.record(
            Some(binding.to_compact_string()),
            &name,
            self.file,
            call.func.range(),
            None,
        );
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
        self.record(
            self.binding.clone(),
            &name,
            self.file,
            keyword.value.range(),
            route.as_deref(),
        );
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

        // the routes belong to the router the registration is made on, which is
        // what an `include(router.urls)` of it will be looking for
        let binding = match &*method.value {
            Expr::Name(router) => Some(router.id.to_compact_string()),
            _ => self.binding.clone(),
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
                binding.clone(),
                &format!("{basename}-{suffix}"),
                self.file,
                anchor,
                Some(prefix.as_str()),
            );
        }

        for action in described.iter().flat_map(|described| &described.actions) {
            // an action's route is that method's, so that is where it leads
            self.record(
                binding.clone(),
                &format!("{basename}-{}", action.url_name),
                action.file,
                action.range,
                Some(prefix.as_str()),
            );
        }
    }

    /// what a registered viewset says about the routes it is given
    fn viewset(&self, viewset: &Expr) -> Option<ViewSet> {
        resolved_class(self.db, self.file, viewset, |file, class| ViewSet {
            basename: default_basename(class),
            actions: actions(class, file),
        })
    }
}

/// the class `expr` names, read in the file that defines it
///
/// following the name to its class reads another module's ast from this
/// module's query, and that cross-file dependency is deliberate: it is what
/// makes an `@action` added to a viewset appear in a template's completions
/// without the url configuration being touched. it costs one file per name
/// followed, which is why it is affordable.
fn resolved_class<T>(
    db: &dyn Db,
    file: File,
    expr: &Expr,
    read: impl Fn(File, &ast::StmtClassDef) -> T,
) -> Option<T> {
    let model = SemanticModel::new(db, file);

    let definitions = match expr {
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
        let defining = definition.file(db);
        let parsed = parsed_module(db, defining).load(db);
        let class = definition.kind(db).as_class()?.node(&parsed);

        Some(read(defining, class))
    })
}

/// whether a router serves an index of what is registered with it
#[derive(Debug, Clone)]
enum RouterRoot {
    /// it does, under this name
    Named(CompactString),
    /// it does not
    None,
    /// nothing reachable says either way
    Unknown,
}

/// what a router built from the class `expr` names calls its index route
///
/// the two rest framework routers are what every other router is written
/// against, so they are what the class is followed up its bases towards. a class
/// naming its own `root_view_name` outranks the one it would inherit.
fn router_root_route(db: &dyn Db, file: File, expr: &Expr, depth: usize) -> RouterRoot {
    let known = match callee_name(expr) {
        Some(class) if class == DEFAULT_ROUTER_CLASS => {
            RouterRoot::Named(ROUTER_ROOT_ROUTE.to_compact_string())
        }
        Some(class) if PLAIN_ROUTER_CLASSES.contains(&class.as_str()) => RouterRoot::None,
        _ => RouterRoot::Unknown,
    };

    if depth == 0 || !matches!(known, RouterRoot::Unknown) {
        return known;
    }

    resolved_class(db, file, expr, |defining, class| {
        let named = class
            .body
            .iter()
            .find_map(|statement| class_attribute(statement, ROUTER_ROOT_VIEW_ATTRIBUTE))
            .and_then(|(value, _)| string_literal(value));

        let mut inherited = RouterRoot::Unknown;
        for base in class.bases() {
            inherited = router_root_route(db, defining, base, depth - 1);
            if !matches!(inherited, RouterRoot::Unknown) {
                break;
            }
        }

        match (inherited, named) {
            (RouterRoot::Named(inherited), named) => RouterRoot::Named(named.unwrap_or(inherited)),
            (inherited, _) => inherited,
        }
    })
    .unwrap_or(RouterRoot::Unknown)
}

/// what an include's first argument mounts
fn include_target(expr: &Expr) -> Option<IncludeTarget> {
    match expr {
        Expr::StringLiteral(literal) => Some(IncludeTarget::Module(
            literal.value.to_str().to_compact_string(),
        )),
        _ => Some(IncludeTarget::Local(local_binding(expr)?)),
    }
}

/// the module-level name an expression is built from, `router` for `router.urls`
fn local_binding(expr: &Expr) -> Option<CompactString> {
    match expr {
        Expr::Name(name) => Some(name.id.to_compact_string()),
        Expr::Attribute(attribute) => local_binding(&attribute.value),
        _ => None,
    }
}

/// the module-level lists a bound value is built out of
///
/// `urlpatterns = router.urls` and `urlpatterns = [*extra, *router.urls]` both
/// mount another of the module's lists under no namespace of their own, which is
/// exactly what an `include()` of that list would do.
fn mounted_lists(expr: &Expr, found: &mut Vec<CompactString>) {
    match expr {
        Expr::Name(_) | Expr::Attribute(_) => found.extend(local_binding(expr)),
        Expr::List(list) => list
            .elts
            .iter()
            .for_each(|element| mounted_lists(element, found)),
        Expr::Tuple(tuple) => tuple
            .elts
            .iter()
            .for_each(|element| mounted_lists(element, found)),
        Expr::Starred(starred) => mounted_lists(&starred.value, found),
        Expr::BinOp(binary) if binary.op == ast::Operator::Add => {
            mounted_lists(&binary.left, found);
            mounted_lists(&binary.right, found);
        }
        _ => {}
    }
}

/// the module-level name a statement binds, and what it binds to it
fn assignment(statement: &Stmt) -> Option<(&str, &Expr)> {
    match statement {
        Stmt::Assign(assign) => match assign.targets.as_slice() {
            [Expr::Name(target)] => Some((target.id.as_str(), &*assign.value)),
            _ => None,
        },
        Stmt::AugAssign(assign) => match &*assign.target {
            Expr::Name(target) => Some((target.id.as_str(), &*assign.value)),
            _ => None,
        },
        Stmt::AnnAssign(assign) => match (&*assign.target, assign.value.as_deref()) {
            (Expr::Name(target), Some(value)) => Some((target.id.as_str(), value)),
            _ => None,
        },
        _ => None,
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
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        let Some((binding, value)) = assignment(stmt) else {
            walk_stmt(self, stmt);
            return;
        };
        let binding = binding.to_compact_string();
        let outer = self.binding.replace(binding.clone());

        let mut mounted = Vec::new();
        mounted_lists(value, &mut mounted);
        for target in mounted {
            self.mounts(IncludeTarget::Local(target), None);
        }

        // a router says which index route it serves where it is built
        if let Expr::Call(call) = value {
            self.router_root(&binding, call);
        }

        walk_stmt(self, stmt);
        self.binding = outer;
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            self.path_call(call);
            self.router_registration(call);
            self.include_call(call);
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

    use super::{
        RegistrationKind, context_for_template, registrations, static_files, tag_libraries,
        template_files, url_names,
    };

    /// a project of python sources, with a throwaway template to anchor the
    /// harness' cursor
    fn project(sources: &[(&str, &str)]) -> TemplateTest {
        let mut all = sources.to_vec();
        all.push(("app/templates/app/page.html", "<CURSOR>"));
        TemplateTest::new(&all)
    }

    /// the `manage.py` a django project points at its settings with
    const MANAGE: (&str, &str) = (
        "manage.py",
        "
        import os

        def main():
            os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'project.settings')
        ",
    );

    /// the `BASE_DIR` preamble every generated settings module opens with
    ///
    /// the settings module sits at `/project/settings.py`, so this is `/`
    const BASE_DIR: &str = "
        from pathlib import Path

        BASE_DIR = Path(__file__).resolve().parent.parent
        ";

    /// a project with a settings module, given as the body that follows `BASE_DIR`
    fn configured(settings: &str, sources: &[(&str, &str)]) -> TemplateTest {
        let settings = format!("{BASE_DIR}\n{settings}");

        let mut all = vec![
            MANAGE,
            ("project/__init__.py", ""),
            ("project/settings.py", &*settings),
        ];
        all.extend_from_slice(sources);

        project(&all)
    }

    /// every template found, as `name -> path`, in the order django would load them
    fn templates(test: &TemplateTest) -> Vec<String> {
        template_files(&test.db, test.db.project())
            .iter()
            .map(|file| format!("{} -> {}", file.name, file.path))
            .collect()
    }

    /// every static file found, as `name -> path`
    fn statics(test: &TemplateTest) -> Vec<String> {
        static_files(&test.db, test.db.project())
            .iter()
            .map(|file| format!("{} -> {}", file.name, file.path))
            .collect()
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

        assert_eq!(names(&test), ["api-root", "book-list", "book-detail"]);
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
            ["api-root", "book-list", "book-detail", "book-mark-read"],
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

        assert_eq!(
            names(&test),
            ["api-root", "book-list", "book-detail", "book-read"]
        );
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

        assert_eq!(names(&test), ["api-root", "book-list", "book-detail"]);
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

        assert_eq!(
            names(&test),
            ["api-root", "book-list", "book-detail", "book-mark-read"]
        );
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

        assert_eq!(names(&test), ["api-root", "book-list", "book-detail"]);
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

        assert_eq!(names(&test), ["api-root", "book-list", "book-detail"]);
    }

    #[test]
    fn a_registration_whose_basename_cannot_be_worked_out_names_nothing() {
        let test = project(&[(
            "app/urls.py",
            "
            class BookViewSet:
                def get_queryset(self): ...

            router = SimpleRouter()
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

        assert_eq!(
            names(&test),
            ["api:api-root", "api:book-list", "api:book-detail"]
        );
    }

    #[test]
    fn a_simple_router_serves_no_index_route() {
        let test = project(&[(
            "app/urls.py",
            "
            class BookViewSet: ...

            router = SimpleRouter()
            router.register('books', BookViewSet, basename='book')
            ",
        )]);

        assert_eq!(
            names(&test),
            ["book-list", "book-detail"],
            "only a `DefaultRouter` serves an index of what is registered with it"
        );
    }

    #[test]
    fn a_router_is_the_one_it_is_written_against() {
        let test = project(&[
            (
                "app/routers.py",
                "
                from rest_framework.routers import DefaultRouter, SimpleRouter

                class LoudRouter(DefaultRouter): ...

                class QuietRouter(SimpleRouter): ...
                ",
            ),
            (
                "app/urls.py",
                "
                from app.routers import LoudRouter, QuietRouter

                class BookViewSet: ...

                loud = LoudRouter()
                loud.register('books', BookViewSet, basename='book')

                quiet = QuietRouter()
                quiet.register('shelves', BookViewSet, basename='shelf')
                ",
            ),
        ]);

        assert_eq!(
            names(&test),
            [
                "api-root",
                "book-list",
                "book-detail",
                "shelf-list",
                "shelf-detail",
            ],
            "a subclass serves the index its base serves, and no other"
        );
    }

    #[test]
    fn a_router_that_names_its_index_route_itself_is_taken_at_its_word() {
        let test = project(&[
            (
                "app/routers.py",
                "
                from rest_framework.routers import DefaultRouter

                class NamedRouter(DefaultRouter):
                    root_view_name = 'index'
                ",
            ),
            (
                "app/urls.py",
                "
                from app.routers import NamedRouter

                router = NamedRouter()
                ",
            ),
        ]);

        assert_eq!(names(&test), ["index"]);
    }

    /// a project whose settings say where its url tree starts
    fn routed(sources: &[(&str, &str)]) -> TemplateTest {
        configured("ROOT_URLCONF = 'project.urls'", sources)
    }

    #[test]
    fn a_name_reached_through_an_include_takes_the_modules_own_app_name() {
        let test = routed(&[
            (
                "project/urls.py",
                "urlpatterns = [path('blog/', include('blog.urls'))]\n",
            ),
            ("blog/__init__.py", ""),
            (
                "blog/urls.py",
                "
                app_name = 'blog'

                urlpatterns = [path('', index, name='index')]
                ",
            ),
        ]);

        assert_eq!(names(&test), ["blog:index"]);
    }

    #[test]
    fn the_namespace_an_include_names_outranks_the_modules_app_name() {
        let test = routed(&[
            (
                "project/urls.py",
                "urlpatterns = [path('blog/', include('blog.urls', namespace='news'))]\n",
            ),
            ("blog/__init__.py", ""),
            (
                "blog/urls.py",
                "
                app_name = 'blog'

                urlpatterns = [path('', index, name='index')]
                ",
            ),
        ]);

        assert_eq!(names(&test), ["news:index"]);
    }

    #[test]
    fn an_include_may_name_its_namespace_by_a_pair() {
        let test = routed(&[
            (
                "project/urls.py",
                "
                urlpatterns = [
                    path('blog/', include(('blog.urls', 'blog'))),
                    path('news/', include(('blog.urls', 'blog'), namespace='news')),
                ]
                ",
            ),
            ("blog/__init__.py", ""),
            (
                "blog/urls.py",
                "urlpatterns = [path('', index, name='index')]\n",
            ),
        ]);

        assert_eq!(
            names(&test),
            ["blog:index", "news:index"],
            "the same module included twice is two namespaces of names"
        );
    }

    #[test]
    fn nested_includes_compose_their_namespaces() {
        let test = routed(&[
            (
                "project/urls.py",
                "urlpatterns = [path('api/', include('api.urls', namespace='api'))]\n",
            ),
            ("api/__init__.py", ""),
            (
                "api/urls.py",
                "urlpatterns = [path('v1/', include('api.v1.urls'))]\n",
            ),
            ("api/v1/__init__.py", ""),
            (
                "api/v1/urls.py",
                "
                app_name = 'v1'

                urlpatterns = [path('books/', listing, name='books')]
                ",
            ),
        ]);

        assert_eq!(names(&test), ["api:v1:books"]);
    }

    #[test]
    fn an_installed_urlconf_is_walked_like_one_of_the_projects_own() {
        // the project's own files are the only ones the flat scan can see, so an
        // include of a package's urlconf is only ever reached through the tree
        let test = installed(
            "
            ROOT_URLCONF = 'project.urls'
            ",
            &[(
                "project/urls.py",
                "urlpatterns = [path('auth/', include('rest_framework.urls'))]\n",
            )],
            &[
                ("rest_framework/__init__.py", ""),
                (
                    "rest_framework/urls.py",
                    "
                    app_name = 'rest_framework'

                    urlpatterns = [
                        path('login/', LoginView, name='login'),
                        path('logout/', LogoutView, name='logout'),
                    ]
                    ",
                ),
            ],
        );

        assert_eq!(
            names(&test),
            ["rest_framework:login", "rest_framework:logout"]
        );
    }

    #[test]
    fn a_router_reached_through_an_include_takes_that_includes_namespace() {
        let test = routed(&[
            (
                "project/urls.py",
                "urlpatterns = [path('api/', include('api.urls', namespace='api'))]\n",
            ),
            ("api/__init__.py", ""),
            (
                "api/urls.py",
                "
                class BookViewSet: ...

                router = DefaultRouter()
                router.register('books', BookViewSet, basename='book')

                urlpatterns = router.urls
                ",
            ),
        ]);

        assert_eq!(
            names(&test),
            ["api:api-root", "api:book-list", "api:book-detail"],
            "the list a module mounts under its own name is mounted with it"
        );
    }

    #[test]
    fn a_list_of_the_module_may_be_included_under_a_namespace_of_its_own() {
        let test = routed(&[
            (
                "project/urls.py",
                "urlpatterns = [path('api/', include('api.urls'))]\n",
            ),
            ("api/__init__.py", ""),
            (
                "api/urls.py",
                "
                class BookViewSet: ...

                router = SimpleRouter()
                router.register('books', BookViewSet, basename='book')

                urlpatterns = [path('v1/', include((router.urls, 'v1')))]
                ",
            ),
        ]);

        assert_eq!(names(&test), ["v1:book-list", "v1:book-detail"]);
    }

    #[test]
    fn a_list_nothing_mounts_names_nothing() {
        let test = routed(&[
            (
                "project/urls.py",
                "urlpatterns = [path('blog/', include('blog.urls'))]\n",
            ),
            ("blog/__init__.py", ""),
            (
                "blog/urls.py",
                "
                urlpatterns = [path('', index, name='index')]

                unused = [path('draft/', draft, name='draft')]
                ",
            ),
        ]);

        assert_eq!(
            names(&test),
            ["index"],
            "django reverses what its tree reaches, and reaches only `urlpatterns`"
        );
    }

    #[test]
    fn a_urlconf_that_includes_itself_still_answers() {
        let test = routed(&[(
            "project/urls.py",
            "
            urlpatterns = [
                path('again/', include('project.urls')),
                path('', index, name='index'),
            ]
            ",
        )]);

        assert_eq!(names(&test), ["index"]);
    }

    #[test]
    fn a_project_without_a_root_urlconf_is_scanned_flat() {
        // the same tree with nothing naming its root: every module that names a
        // route contributes, under its own `app_name`, and no include is followed
        let test = configured(
            "
            INSTALLED_APPS = ['blog']
            ",
            &[
                (
                    "project/urls.py",
                    "urlpatterns = [path('blog/', include('blog.urls', namespace='news'))]\n",
                ),
                ("blog/__init__.py", ""),
                (
                    "blog/urls.py",
                    "
                    app_name = 'blog'

                    urlpatterns = [path('', index, name='index')]
                    ",
                ),
            ],
        );

        assert_eq!(
            names(&test),
            ["blog:index"],
            "the include's namespace is unknown without the tree that applies it"
        );
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
    fn a_directory_the_settings_name_holds_templates_the_convention_never_finds() {
        let test = configured(
            "
            TEMPLATES = [{'DIRS': [BASE_DIR / 'shared'], 'APP_DIRS': True}]
            ",
            &[("shared/site/banner.html", "")],
        );

        assert_eq!(
            templates(&test),
            [
                "app/page.html -> /app/templates/app/page.html",
                "site/banner.html -> /shared/site/banner.html",
            ],
            "the settings' directory adds to what the convention already found"
        );
    }

    #[test]
    fn a_directory_may_be_named_by_a_plain_string() {
        let test = configured(
            "
            TEMPLATES = [{'DIRS': ['shared']}]
            ",
            &[("shared/site/banner.html", "")],
        );

        assert!(templates(&test).contains(&"site/banner.html -> /shared/site/banner.html".into()));
    }

    #[test]
    fn a_directory_may_be_joined_the_os_path_way() {
        let test = configured(
            "
            import os

            TEMPLATES = [{'DIRS': [os.path.join(BASE_DIR, 'shared')]}]
            ",
            &[("shared/site/banner.html", "")],
        );

        assert!(templates(&test).contains(&"site/banner.html -> /shared/site/banner.html".into()));
    }

    #[test]
    fn a_directory_that_cannot_be_worked_out_is_skipped_and_nothing_else_is() {
        let test = configured(
            "
            import os

            TEMPLATES = [{'DIRS': [os.environ['TEMPLATE_ROOT'], BASE_DIR / 'shared']}]
            ",
            &[("shared/site/banner.html", "")],
        );

        assert_eq!(
            templates(&test),
            [
                "app/page.html -> /app/templates/app/page.html",
                "site/banner.html -> /shared/site/banner.html",
            ],
            "an unresolvable directory costs only itself"
        );
    }

    #[test]
    fn a_project_without_a_settings_module_is_discovered_by_convention_alone() {
        // the same settings, with nothing pointing at them: `manage.py` is what
        // makes a settings module the project's, and a directory only it names
        // must stay unknown
        let settings = format!("{BASE_DIR}\nTEMPLATES = [{{'DIRS': [BASE_DIR / 'shared']}}]");
        let test = project(&[
            ("project/__init__.py", ""),
            ("project/settings.py", &settings),
            ("shared/site/banner.html", ""),
        ]);

        assert_eq!(
            templates(&test),
            ["app/page.html -> /app/templates/app/page.html"]
        );
    }

    #[test]
    fn installed_apps_decides_which_of_two_same_named_templates_leads() {
        let test = configured(
            "
            INSTALLED_APPS = ['second', 'first']

            TEMPLATES = [{'APP_DIRS': True}]
            ",
            &[
                ("first/__init__.py", ""),
                ("first/templates/base.html", ""),
                ("second/__init__.py", ""),
                ("second/templates/base.html", ""),
            ],
        );

        assert_eq!(
            templates(&test),
            [
                "app/page.html -> /app/templates/app/page.html",
                "base.html -> /second/templates/base.html",
                "base.html -> /first/templates/base.html",
            ],
            "the app installed first is the one django's loader reaches first"
        );
    }

    #[test]
    fn an_app_may_be_installed_by_the_config_class_inside_it() {
        let test = configured(
            "
            INSTALLED_APPS = ['second.apps.SecondConfig', 'first']

            TEMPLATES = [{'APP_DIRS': True}]
            ",
            &[
                ("first/__init__.py", ""),
                ("first/templates/base.html", ""),
                ("second/__init__.py", ""),
                ("second/apps.py", "class SecondConfig: ...\n"),
                ("second/templates/base.html", ""),
            ],
        );

        assert_eq!(
            templates(&test)[1],
            "base.html -> /second/templates/base.html",
            "the app is the package the config lives in"
        );
    }

    #[test]
    fn without_the_app_directories_loader_no_app_leads_another() {
        // django would search none of them, so nothing is known about their
        // order and they keep the one they already had
        let test = configured(
            "
            INSTALLED_APPS = ['second', 'first']

            TEMPLATES = [{'DIRS': []}]
            ",
            &[
                ("first/__init__.py", ""),
                ("first/templates/base.html", ""),
                ("second/__init__.py", ""),
                ("second/templates/base.html", ""),
            ],
        );

        assert_eq!(
            templates(&test)[1],
            "base.html -> /first/templates/base.html",
            "the app directories are still discovered, just not ordered"
        );
    }

    #[test]
    fn a_named_directory_leads_the_app_that_holds_the_same_name() {
        let test = configured(
            "
            INSTALLED_APPS = ['first']

            TEMPLATES = [{'DIRS': [BASE_DIR / 'shared'], 'APP_DIRS': True}]
            ",
            &[
                ("first/__init__.py", ""),
                ("first/templates/base.html", ""),
                ("shared/base.html", ""),
            ],
        );

        assert_eq!(
            templates(&test)[1],
            "base.html -> /shared/base.html",
            "django tries `DIRS` before it tries the installed apps"
        );
    }

    #[test]
    fn manage_py_decides_where_a_wsgi_module_disagrees() {
        let test = project(&[
            MANAGE,
            (
                "project/wsgi.py",
                "
                import os

                os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'project.production')
                ",
            ),
            ("project/__init__.py", ""),
            (
                "project/settings.py",
                "TEMPLATES = [{'DIRS': ['shared']}]\n",
            ),
            (
                "project/production.py",
                "TEMPLATES = [{'DIRS': ['other']}]\n",
            ),
            ("shared/site/banner.html", ""),
            ("other/site/other.html", ""),
        ]);

        let found = templates(&test);
        assert!(found.contains(&"site/banner.html -> /shared/site/banner.html".into()));
        assert!(
            !found.iter().any(|file| file.contains("other.html")),
            "the settings `manage.py` names are the ones read"
        );
    }

    #[test]
    fn a_settings_module_named_by_a_subscript_is_found_too() {
        let test = project(&[
            (
                "manage.py",
                "
                import os

                os.environ['DJANGO_SETTINGS_MODULE'] = 'project.settings'
                ",
            ),
            ("project/__init__.py", ""),
            (
                "project/settings.py",
                "TEMPLATES = [{'DIRS': ['shared']}]\n",
            ),
            ("shared/site/banner.html", ""),
        ]);

        assert!(templates(&test).contains(&"site/banner.html -> /shared/site/banner.html".into()));
    }

    #[test]
    fn staticfiles_dirs_are_discovered_the_same_way() {
        let test = configured(
            "
            STATICFILES_DIRS = [BASE_DIR / 'assets']
            ",
            &[("assets/site.css", ""), ("app/static/app/app.css", "")],
        );

        assert_eq!(
            statics(&test),
            [
                "app/app.css -> /app/static/app/app.css",
                "site.css -> /assets/site.css",
            ]
        );
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

    /// a project with a settings module and packages installed beside it
    ///
    /// the packages go to a site-packages directory outside the project root,
    /// which is what makes them third-party rather than the project's own.
    fn installed(
        settings: &str,
        sources: &[(&str, &str)],
        packages: &[(&str, &str)],
    ) -> TemplateTest {
        let settings = format!("{BASE_DIR}\n{settings}");

        let mut all = vec![
            MANAGE,
            ("project/__init__.py", ""),
            ("project/settings.py", &*settings),
            ("app/templates/app/page.html", "<CURSOR>"),
        ];
        all.extend_from_slice(sources);

        TemplateTest::with_site_packages(&all, packages)
    }

    /// a mock django, with the `humanize` contrib app and the `i18n` library
    ///
    /// `i18n` is written the way django writes it, with the registered name given
    /// to the decorator rather than taken from the function.
    const DJANGO: &[(&str, &str)] = &[
        ("django/__init__.py", ""),
        ("django/templatetags/__init__.py", ""),
        (
            "django/templatetags/i18n.py",
            "
            from django.template import Library

            register = Library()

            @register.tag('translate')
            def do_translate(parser, token): ...

            @register.tag('localize')
            def localize_tag(parser, token): ...
            ",
        ),
        ("django/contrib/__init__.py", ""),
        ("django/contrib/humanize/__init__.py", ""),
        ("django/contrib/humanize/templatetags/__init__.py", ""),
        (
            "django/contrib/humanize/templatetags/humanize.py",
            "
            from django.template import Library

            register = Library()

            @register.filter
            def intcomma(value):
                '''adds thousand separators.'''
                return value

            @register.filter(is_safe=True)
            def naturaltime(value):
                return value
            ",
        ),
        // a stub beside the runtime module, which is what a `django-stubs`
        // installation puts there and which registers nothing
        (
            "django/contrib/humanize/templatetags/humanize.pyi",
            "def intcomma(value: object) -> str: ...\n",
        ),
    ];

    /// every tag library found, as `name (source)`, always-loaded ones marked
    fn libraries(test: &TemplateTest) -> Vec<String> {
        tag_libraries(&test.db, test.db.project())
            .iter()
            .map(|library| {
                format!(
                    "{} ({:?}{})",
                    library.name,
                    library.source,
                    if library.always_loaded {
                        ", always loaded"
                    } else {
                        ""
                    }
                )
            })
            .collect()
    }

    /// every registration found, as `library|name`, django's own marked
    fn registered(test: &TemplateTest) -> Vec<String> {
        registrations(&test.db, test.db.project())
            .iter()
            .map(|registration| {
                format!(
                    "{}|{}{}",
                    registration.library,
                    registration.name,
                    if registration.django { " (django)" } else { "" }
                )
            })
            .collect()
    }

    #[test]
    fn a_contrib_apps_library_is_discovered_and_is_djangos() {
        let test = installed(
            "
            INSTALLED_APPS = ['django.contrib.humanize']
            ",
            &[],
            DJANGO,
        );

        assert_eq!(
            libraries(&test),
            ["i18n (Django)", "humanize (Django)"],
            "django's own `django.templatetags` is a candidate however the apps are configured"
        );
        assert_eq!(
            registered(&test),
            [
                "i18n|translate (django)",
                "i18n|localize (django)",
                "humanize|intcomma (django)",
                "humanize|naturaltime (django)",
            ]
        );
    }

    #[test]
    fn a_contrib_apps_library_leads_to_the_module_that_runs_rather_than_the_stub_beside_it() {
        let test = installed(
            "
            INSTALLED_APPS = ['django.contrib.humanize']
            ",
            &[],
            DJANGO,
        );

        let intcomma = registrations(&test.db, test.db.project())
            .iter()
            .find(|registration| registration.name == "intcomma")
            .expect("the filter to be discovered");

        assert_eq!(
            intcomma.file.path(&test.db).to_string(),
            "/site-packages/django/contrib/humanize/templatetags/humanize.py"
        );
    }

    #[test]
    fn a_third_party_apps_library_is_discovered_but_is_not_djangos() {
        let test = installed(
            "
            INSTALLED_APPS = ['crispy_forms']
            ",
            &[],
            &[
                ("crispy_forms/__init__.py", ""),
                ("crispy_forms/templatetags/__init__.py", ""),
                (
                    "crispy_forms/templatetags/crispy_forms_tags.py",
                    "
                    from django import template

                    register = template.Library()

                    @register.filter
                    def as_crispy_field(field):
                        return field
                    ",
                ),
            ],
        );

        assert_eq!(libraries(&test), ["crispy_forms_tags (Installed)"]);
        assert_eq!(registered(&test), ["crispy_forms_tags|as_crispy_field"]);
    }

    #[test]
    fn an_app_installed_by_its_config_class_brings_its_library_all_the_same() {
        let test = installed(
            "
            INSTALLED_APPS = ['crispy_forms.apps.CrispyFormsConfig']
            ",
            &[],
            &[
                ("crispy_forms/__init__.py", ""),
                ("crispy_forms/apps.py", "class CrispyFormsConfig: ...\n"),
                ("crispy_forms/templatetags/__init__.py", ""),
                (
                    "crispy_forms/templatetags/crispy_forms_tags.py",
                    "
                    from django import template

                    register = template.Library()

                    @register.filter
                    def as_crispy_field(field):
                        return field
                    ",
                ),
            ],
        );

        assert_eq!(libraries(&test), ["crispy_forms_tags (Installed)"]);
    }

    #[test]
    fn an_installed_app_without_templatetags_contributes_nothing() {
        let test = installed(
            "
            INSTALLED_APPS = ['plain_app', 'missing_app']
            ",
            &[],
            &[("plain_app/__init__.py", ""), ("plain_app/models.py", "")],
        );

        assert!(
            libraries(&test).is_empty(),
            "neither an app without a `templatetags` package nor one that isn't there at all"
        );
    }

    #[test]
    fn the_projects_own_library_is_reported_as_its_own_even_when_it_is_installed() {
        let test = installed(
            "
            INSTALLED_APPS = ['app']
            ",
            &[
                ("app/__init__.py", ""),
                ("app/templatetags/__init__.py", ""),
                (
                    "app/templatetags/app_extras.py",
                    "
                    from django import template

                    register = template.Library()

                    @register.filter
                    def shout(value):
                        return value
                    ",
                ),
            ],
            &[],
        );

        assert_eq!(
            libraries(&test),
            ["app_extras (Project)"],
            "one module is one library however many ways it was reached"
        );
        assert_eq!(registered(&test), ["app_extras|shout"]);
    }

    #[test]
    fn a_library_the_settings_load_into_every_template_needs_no_load() {
        let test = installed(
            "
            INSTALLED_APPS = []

            TEMPLATES = [{
                'OPTIONS': {'builtins': ['everywhere.templatetags.everywhere_tags']},
            }]
            ",
            &[],
            &[
                ("everywhere/__init__.py", ""),
                ("everywhere/templatetags/__init__.py", ""),
                (
                    "everywhere/templatetags/everywhere_tags.py",
                    "
                    from django import template

                    register = template.Library()

                    @register.simple_tag
                    def banner():
                        return ''
                    ",
                ),
            ],
        );

        assert_eq!(
            libraries(&test),
            ["everywhere_tags (Installed, always loaded)"],
            "the app it lives in need not be installed for its tags to be everywhere"
        );
        assert_eq!(registered(&test), ["everywhere_tags|banner"]);
    }

    #[test]
    fn an_installed_apps_library_may_be_loaded_into_every_template_as_well() {
        let test = installed(
            "
            INSTALLED_APPS = ['django.contrib.humanize']

            TEMPLATES = [{
                'OPTIONS': {'builtins': ['django.contrib.humanize.templatetags.humanize']},
            }]
            ",
            &[],
            DJANGO,
        );

        assert_eq!(
            libraries(&test),
            ["i18n (Django)", "humanize (Django, always loaded)"],
            "the two ways of reaching one module fold into one library"
        );
    }

    #[test]
    fn a_project_without_settings_discovers_only_its_own_libraries() {
        // the same packages installed, with nothing pointing at a settings
        // module: what django would load is unknown, so the convention alone
        // answers and nothing third-party is reached
        let test = TemplateTest::with_site_packages(
            &[
                ("app/templates/app/page.html", "<CURSOR>"),
                ("app/templatetags/__init__.py", ""),
                (
                    "app/templatetags/app_extras.py",
                    "
                    from django import template

                    register = template.Library()

                    @register.filter
                    def shout(value):
                        return value
                    ",
                ),
            ],
            DJANGO,
        );

        assert_eq!(libraries(&test), ["app_extras (Project)"]);
        assert_eq!(registered(&test), ["app_extras|shout"]);
    }

    #[test]
    fn a_discovered_library_beats_the_table_where_they_disagree() {
        // the builtin table puts `{% localize %}` in `l10n`, written against one
        // version of django; this project's own django registers it in `i18n`,
        // and it is the project's django that is right about the project
        let test = installed(
            "
            INSTALLED_APPS = []
            ",
            &[],
            DJANGO,
        );

        assert!(
            super::super::builtins::tag("localize").is_some_and(|tag| tag.library == Some("l10n"))
        );
        assert!(registered(&test).contains(&"i18n|localize (django)".to_string()));
    }
}
