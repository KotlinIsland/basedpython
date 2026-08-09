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
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::{FxHashMap, FxHashSet};
use ty_module_resolver::{
    Module, ModuleName, file_to_module, resolve_module_confident, resolve_real_module,
};
use ty_project::{Db, Project};
use ty_python_core::definition::DefinitionKind;
use ty_python_semantic::SemanticModel;
use ty_python_semantic::django_settings::{self as settings_source, SettingsNaming};
use ty_python_semantic::types::ide_support::{
    ImportAliasResolution, ResolvedDefinition, definitions_for_attribute, definitions_for_name,
    instance_of_class,
};
use ty_python_semantic::types::{KnownClass, Type};

/// the directory name django's app-directories template loader looks in
const TEMPLATE_DIRECTORY: &str = "templates";

/// the directory name django's `staticfiles` app-directories finder looks in
const STATIC_DIRECTORY: &str = "static";

/// the package name a project's template tag libraries live in
const TEMPLATETAGS_PACKAGE: &str = "templatetags";

/// the package that declares the base every test case descends from
const UNITTEST_PACKAGE: &str = "unittest";

/// the base every test case descends from
///
/// django's own `SimpleTestCase` and everything below it are written against it,
/// and django's test runner discovers by it, so it identifies a test class
/// whether the class was written against django's bases or against plain
/// `unittest`'s.
const TEST_CASE_BASE: &str = "TestCase";

/// the prefix django's test runner discovers test methods by
///
/// this is `unittest`'s own `testMethodPrefix`, which django does not change.
pub(crate) const TEST_METHOD_PREFIX: &str = "test";

/// the package a django app holds its migrations in
pub(crate) const MIGRATIONS_PACKAGE: &str = "migrations";

/// the module a django app declares its models in
pub(crate) const MODELS_MODULE: &str = "models";

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

/// the option an engine replaces the default template loaders with
const LOADERS_OPTION: &str = "loaders";

/// the option naming the functions every render merges the result of into its context
const CONTEXT_PROCESSORS_OPTION: &str = "context_processors";

/// the keyword a render call passes its context by
const CONTEXT_KEYWORD: &str = "context";

/// the class attribute a generic view names the object it renders by
const CONTEXT_OBJECT_NAME_ATTRIBUTE: &str = "context_object_name";

/// the class attribute a view adds fixed names to its context through
const EXTRA_CONTEXT_ATTRIBUTE: &str = "extra_context";

/// the method a class-based view builds its context in
const GET_CONTEXT_DATA_METHOD: &str = "get_context_data";

/// the method a context dict is extended in place with
const CONTEXT_UPDATE_METHOD: &str = "update";

/// how many names deep a context held in a variable is followed
///
/// each hop is a `{**base}` spread or a name bound to another, and a project
/// nests a couple at most. bounding it is what keeps a context built out of
/// itself from being followed for ever.
const MAX_CONTEXT_DEPTH: usize = 4;

/// how many base classes deep a view's context is collected from
const MAX_VIEW_DEPTH: usize = 8;

/// django's own package, whose `templatetags` is a library candidate like any
/// installed app's
const DJANGO_PACKAGE: &str = "django";

/// the modules every template engine starts with loaded
///
/// this is django's `Engine.default_builtins`, and it is where `{% for %}`,
/// `{% extends %}` and `|upper` come from. they are libraries like any other
/// here; the only difference is that nothing has to `{% load %}` them.
const DEFAULT_BUILTIN_MODULES: &[&str] = &[
    "django.template.defaulttags",
    "django.template.defaultfilters",
    "django.template.loader_tags",
];

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

/// the keyword those functions take the route name by
pub(crate) const REVERSE_NAME_KEYWORD: &str = "viewname";

/// the keyword those functions take the route's own arguments by
pub(crate) const REVERSE_ARGUMENTS_KEYWORD: &str = "kwargs";

/// the keyword a rest framework hyperlinked field names its route by
///
/// a `HyperlinkedIdentityField(view_name="blog:detail")` reverses a route as
/// surely as a `reverse()` does, and the keyword is distinctive enough to be
/// read on sight wherever the field is built.
const HYPERLINK_NAME_KEYWORD: &str = "view_name";

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
pub(crate) const NAMESPACE_SEPARATOR: char = ':';

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

/// django's own base class every model is written against
const MODEL_BASE: &str = "Model";

/// django's own base class every admin class is written against
///
/// an inline is deliberately not this: `TabularInline` and `StackedInline`
/// descend from `InlineModelAdmin`, which is no `ModelAdmin`, and an inline is
/// reached by another admin class's `inlines` rather than by a registration.
const MODEL_ADMIN_BASE: &str = "ModelAdmin";

/// the method an admin site registers a model with
const ADMIN_REGISTER_METHOD: &str = "register";

/// the keyword a registration names the admin class by
const ADMIN_CLASS_KEYWORD: &str = "admin_class";

/// the method a class-based view is handed to a route through
const AS_VIEW_METHOD: &str = "as_view";

/// how many base classes deep a class is followed towards django's own
///
/// a project's own hierarchy is a couple deep and django's own is another
/// couple, so this is far past anything real — it is here so that a class that
/// somehow inherits from itself stops the walk rather than running for ever.
const MAX_BASE_DEPTH: usize = 16;

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
    /// whether the library it comes from is loaded into every template already
    pub(crate) always_loaded: bool,
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
    /// the route pattern, every prefix it is mounted under included
    ///
    /// `None` where some part of the pattern could not be read, which is the
    /// difference between a route that takes no arguments and one whose
    /// arguments are unknown.
    pub(crate) route: Option<Box<str>>,
    /// whether `route` is the whole pattern django reverses against
    ///
    /// a rest framework router generates its routes from a prefix rather than
    /// writing them out, so what one of them takes cannot be read off `route`.
    pub(crate) exact: bool,
    /// the view the route hands the request to, where it names one this can read
    pub(crate) view: Option<RouteView>,
    /// whether the route hands the view arguments its pattern does not name
    ///
    /// `path()` takes a dict of extra keyword arguments django passes on top of
    /// whatever the pattern captured, so a view with a parameter the pattern says
    /// nothing about may still be called with one.
    pub(crate) extra_arguments: bool,
}

/// the view a route hands the request to
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct RouteView {
    /// where the view is declared
    pub(crate) target: Target,
    /// where the route names it, for a diagnostic about the pairing to point at
    pub(crate) range: TextRange,
    /// whether the route reaches it through `as_view()`
    ///
    /// django calls what `as_view()` returns rather than the class itself, so a
    /// class reached that way serves the request through its handler methods
    /// rather than through anything the class declares directly.
    pub(crate) class_based: bool,
}

/// what a python definition a django construct names is
#[derive(Debug, Clone, Copy, PartialEq, Eq, get_size2::GetSize)]
pub(crate) enum TargetKind {
    Function,
    Class,
}

/// a python definition a django construct names, wherever it is written
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct Target {
    pub(crate) name: CompactString,
    pub(crate) kind: TargetKind,
    pub(crate) file: File,
    /// the name it is declared under, for navigation
    pub(crate) range: TextRange,
    /// the whole declaration
    pub(crate) full_range: TextRange,
}

impl UrlName {
    /// the arguments this route takes, or `None` where they cannot be read
    pub(crate) fn parameters(&self) -> Option<Vec<Parameter>> {
        self.exact
            .then_some(self.route.as_deref()?)
            .and_then(|route| parameters_of(route, DJANGO_SYNTAX))
    }
}

/// how a framework delimits the parameters it writes into a route pattern
///
/// django writes `<int:pk>`; the brace form fastapi, starlette and django-bolt
/// share writes `{pk}` or `{pk:int}`, the name and the converter the other way
/// round. beyond the delimiters and that order, reading a pattern is the same
/// work either way, so this is all a framework has to say about its spelling —
/// what its converter names mean is [`Converter`]'s to say.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ParameterSyntax {
    /// what opens a parameter
    open: char,
    /// what closes it
    close: char,
    /// whether the converter is written before the name, as django writes it
    converter_first: bool,
    /// whether a regular expression's named groups name parameters too
    ///
    /// django's `re_path()` takes a regular expression rather than a pattern of
    /// its own, and a named group in one is an argument like any other — put
    /// through no converter, so nothing is known about its type.
    named_groups: bool,
}

/// how django's `path()` and `re_path()` write their parameters
const DJANGO_SYNTAX: ParameterSyntax = ParameterSyntax {
    open: '<',
    close: '>',
    converter_first: true,
    named_groups: true,
};

/// one argument a route pattern takes
pub(crate) struct Parameter {
    pub(crate) name: CompactString,
    /// the converter django puts the argument through, where it is one of its own
    pub(crate) converter: Option<Converter>,
}

impl Parameter {
    /// the python value the framework hands the view for this argument
    ///
    /// `None` where nothing here says: an argument matched by a regular
    /// expression goes through no converter at all, and one whose converter the
    /// project registered itself yields whatever that converter's `to_python`
    /// returns.
    pub(crate) fn value_type<'db>(&self, db: &'db dyn Db) -> Option<Type<'db>> {
        self.converter?.value_type(db)
    }
}

/// the path converters django ships
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Converter {
    Str,
    Int,
    Slug,
    Uuid,
    Path,
}

impl Converter {
    fn of(name: &str) -> Option<Self> {
        match name {
            "str" => Some(Self::Str),
            "int" => Some(Self::Int),
            "slug" => Some(Self::Slug),
            "uuid" => Some(Self::Uuid),
            "path" => Some(Self::Path),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Str => "str",
            Self::Int => "int",
            Self::Slug => "slug",
            Self::Uuid => "uuid",
            Self::Path => "path",
        }
    }

    /// the python value the view is handed for an argument this matched
    ///
    /// this is the other half of what a converter is: [`Self::matches`] says what
    /// the url may hold, and this says what comes out of `to_python` on the other
    /// side.
    fn value_type(self, db: &dyn Db) -> Option<Type<'_>> {
        match self {
            // three of django's five converters differ only in what they match
            Self::Str | Self::Slug | Self::Path => Some(KnownClass::Str.to_instance(db)),
            Self::Int => Some(KnownClass::Int.to_instance(db)),
            Self::Uuid => instance_of_class(db, "uuid", "UUID"),
        }
    }

    /// whether a value written out in the template is one this would match
    pub(crate) fn matches(self, value: &str) -> bool {
        match self {
            Self::Str => !value.is_empty() && !value.contains('/'),
            Self::Int => !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()),
            Self::Slug => {
                !value.is_empty()
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            }
            Self::Uuid => {
                let groups: Vec<&str> = value.split('-').collect();
                groups.len() == 5
                    && [8, 4, 4, 4, 12]
                        == *groups.iter().map(|group| group.len()).collect::<Vec<_>>()
                    // django's own pattern is lowercase, and `str(UUID(…))` is
                    // the only spelling that reaches it at run time
                    && value
                        .bytes()
                        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f' | b'-'))
            }
            Self::Path => !value.is_empty(),
        }
    }
}

/// the arguments a route pattern names, read in the spelling `syntax` describes
///
/// django writes them two ways: `path()` takes `<converter:name>` and
/// `re_path()` takes a named group. a pattern with an *unnamed* group takes
/// arguments nothing can name, so it answers nothing rather than too few.
fn parameters_of(pattern: &str, syntax: ParameterSyntax) -> Option<Vec<Parameter>> {
    let mut parameters = Vec::new();
    let mut rest = pattern;

    let opener: &[char] = if syntax.named_groups {
        &[syntax.open, '(']
    } else {
        std::slice::from_ref(&syntax.open)
    };

    while let Some(index) = rest.find(opener) {
        let after = &rest[index..];

        if let Some(after) = after.strip_prefix(syntax.open) {
            let (declaration, tail) = after.split_once(syntax.close)?;
            let (converter, name) = match declaration.split_once(':') {
                Some((converter, name)) if syntax.converter_first => {
                    (Converter::of(converter), name)
                }
                Some((name, converter)) => (Converter::of(converter), name),
                // a parameter written without a converter goes through the one
                // that matches any single path segment
                None => (Some(Converter::Str), declaration),
            };
            parameters.push(Parameter {
                name: name.to_compact_string(),
                converter,
            });
            rest = tail;
            continue;
        }

        let after = after.strip_prefix('(').unwrap_or(after);
        if let Some(after) = after.strip_prefix("?P<") {
            let (name, tail) = after.split_once('>')?;
            parameters.push(Parameter {
                name: name.to_compact_string(),
                // what a regex group matches is not one of django's converters,
                // so a literal written against it is not checked
                converter: None,
            });
            rest = tail;
            continue;
        }

        // a group that captures without naming takes a positional argument this
        // has no name for, so the pattern is one to say nothing about
        if !after.starts_with("?:") && !after.starts_with("?=") && !after.starts_with("?!") {
            return None;
        }
        rest = after;
    }

    Some(parameters)
}

/// the arguments every route of `name` takes, in the order they are declared
///
/// django allows two routes to share a name and reverses against whichever one
/// matches, so the union is what such a name takes rather than either half.
pub(crate) fn route_parameters(db: &dyn Db, name: &str) -> Vec<Parameter> {
    let mut parameters: Vec<Parameter> = Vec::new();

    for url in url_names(db, db.project())
        .iter()
        .filter(|url| url.name == name)
    {
        for parameter in url.parameters().into_iter().flatten() {
            if !parameters.iter().any(|known| known.name == parameter.name) {
                parameters.push(parameter);
            }
        }
    }

    parameters
}

/// where a name in a template's context comes from
#[derive(Debug, Clone, Copy, PartialEq, Eq, get_size2::GetSize)]
pub(crate) enum ContextSource {
    /// a view put it in this template's context
    View,
    /// a context processor puts it in every template's context
    Processor,
}

impl ContextSource {
    /// how the name's provenance reads, for a name nothing gives a type to
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::View => "from the view's context",
            Self::Processor => "from a context processor",
        }
    }
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
    pub(crate) source: ContextSource,
}

/// the context one template is rendered with, from one place that renders it
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct TemplateContext {
    /// the template the view names, as the loader sees it
    pub(crate) template: CompactString,
    /// the view that renders it, where the name is written inside one
    ///
    /// a `render()` at module level is nobody's view, and a template rendered
    /// only from there has none to name.
    pub(crate) view: Option<ViewRef>,
    pub(crate) variables: Box<[ContextVariable]>,
}

/// the definition a template name is written inside
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct ViewRef {
    /// the definition's name, qualified by whatever it is nested in
    ///
    /// a view written inside another definition carries both names, since
    /// `outer.inner` is what a reader is looking for and `inner` alone is not.
    /// the module is deliberately not part of it: which module a file is comes
    /// from the module resolver rather than from this scan.
    pub(crate) path: CompactString,
    pub(crate) file: File,
    /// the innermost definition's own name, for navigation
    pub(crate) range: TextRange,
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
    /// whether django loads it out of a directory of the project's own
    ///
    /// an installed app in site-packages holds templates django loads exactly as
    /// it loads the project's, and they are equally real to everything that only
    /// reads them — but they are not the project's to rewrite.
    pub(crate) own: bool,
}

impl DiscoveredFile {
    /// the directory the file's name is relative to
    ///
    /// this is the directory django's loader searched to find it, which is where
    /// a file renamed to another name would have to end up.
    pub(crate) fn root(&self) -> Option<&SystemPath> {
        self.path
            .ancestors()
            .nth(self.name.matches('/').count() + 1)
    }
}

/// whether the project has django at all
///
/// a project without it has no django templates: an `.html` file under a
/// `templates` directory of a flask or a jinja project is not one, and every
/// check reads something — the tag libraries, the template directories, the url
/// tree — that django is what defines. asking first is also what keeps a project
/// with no django from paying for the file-system walks the discovery does.
#[salsa::tracked(returns(copy))]
pub(crate) fn has_django(db: &dyn Db, _project: Project) -> bool {
    ModuleName::new_static(DJANGO_PACKAGE)
        .and_then(|name| resolve_module_confident(db, &name))
        .is_some()
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

/// whether everything the settings say about the project was worked out
///
/// what the templates, the static files and the tag libraries are discovered
/// from is a union of what the convention finds and what the settings name,
/// which is the right answer for a *suggestion*: a directory too many costs a
/// completion nobody wanted. a diagnostic needs the opposite guarantee, that
/// nothing is missing, and only a settings module read all the way through gives
/// it — without one there is no `INSTALLED_APPS`, and without that an installed
/// app's own templates and libraries are somewhere nothing here will ever look.
#[salsa::tracked(returns(copy))]
pub(crate) fn settings_are_authoritative(db: &dyn Db, project: Project) -> bool {
    let Some(importing) = *settings_file(db, project) else {
        return false;
    };
    let settings = django_settings(db, project);

    settings.read_in_full
        && settings
            .installed_apps
            .iter()
            .all(|app| app_package(db, importing, app).is_some())
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
                    always_loaded: library.always_loaded,
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

    // django's own implicit builtins, which no template has to load and which
    // nothing above reaches: they live in `django.template`, not in anybody's
    // `templatetags`. reading them is what stops the builtin table from being the
    // last word on a django whose tags have moved on since it was written
    for path in DEFAULT_BUILTIN_MODULES {
        if let Some(discovered) = always_loaded_library(db, importing, path, LibrarySource::Django)
        {
            merge(&mut found, discovered);
        }
    }

    // a library the settings load into every template is available whether or not
    // any app installs it, and needs no `{% load %}` wherever it came from
    for path in &settings.always_loaded {
        let source = if path.split('.').next() == Some(DJANGO_PACKAGE) {
            LibrarySource::Django
        } else {
            LibrarySource::Installed
        };

        if let Some(discovered) = always_loaded_library(db, importing, path, source) {
            merge(&mut found, discovered);
        }
    }

    found.into_boxed_slice()
}

/// the library a dotted module path names, as one no template has to `{% load %}`
fn always_loaded_library(
    db: &dyn Db,
    importing: File,
    path: &str,
    source: LibrarySource,
) -> Option<Library> {
    let name = ModuleName::new(path)?;
    let file = resolve_real_module(db, importing, &name)?.file(db)?;

    library(db, file, source, true)
}

/// whether django's own registrations were read, and so are the last word on
/// what django provides
///
/// the implicit builtins are the surest sign of a django this module can
/// actually read: `Engine` imports all three itself, so a project where all
/// three resolve is one whose django is really there. short of that, nothing of
/// django's has been read at all and the builtin table stands on its own — which
/// is the difference between a table that is a floor and a table that is a
/// claim.
#[salsa::tracked(returns(copy))]
pub(crate) fn django_is_authoritative(db: &dyn Db, project: Project) -> bool {
    let Some(importing) = *settings_file(db, project) else {
        return false;
    };

    DEFAULT_BUILTIN_MODULES.iter().all(|path| {
        ModuleName::new(path)
            .and_then(|name| resolve_real_module(db, importing, &name))
            .and_then(|module| module.file(db))
            .is_some()
    })
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
    walk_urls(db, project).map_or_else(
        || project_scan(db, project, flat_url_names),
        |walk| walk.found.into_boxed_slice(),
    )
}

/// whether the route names are the ones django would reverse, and all of them
///
/// only a walk from `ROOT_URLCONF` puts a name under the namespace django gives
/// it, and only a walk that got all the way through every include it met has
/// seen every name there is. anything short of that is a set of names a "no such
/// route" would be wrong about, so it answers nothing rather than something
/// plausible.
#[salsa::tracked(returns(copy))]
pub(crate) fn routes_are_authoritative(db: &dyn Db, project: Project) -> bool {
    walk_urls(db, project).is_some_and(|walk| walk.complete)
}

/// the url tree walked from `ROOT_URLCONF`, for a project that names one
fn walk_urls(db: &dyn Db, project: Project) -> Option<UrlWalk<'_>> {
    let root = (*root_urlconf(db, project))?;

    let mut walk = UrlWalk {
        db,
        found: Vec::new(),
        visited: FxHashSet::default(),
        complete: true,
    };
    walk.mount(root, URLPATTERNS_VARIABLE, "", Some(""), MAX_URLCONF_DEPTH);

    let mut seen = FxHashSet::default();
    walk.found
        .retain(|url| seen.insert((url.name.clone(), url.file, url.range)));

    Some(walk)
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
            UrlEntryKind::Route(route) => Some(mounted(route, &namespace, Some(""))),
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
    /// whether every module the walk met was read all the way through
    complete: bool,
}

impl UrlWalk<'_> {
    /// walk the routes `binding` of `file` holds, mounted under `namespace` and
    /// behind the route pattern `prefix`
    fn mount(
        &mut self,
        file: File,
        binding: &str,
        namespace: &str,
        prefix: Option<&str>,
        depth: usize,
    ) {
        if depth == 0 {
            // a tree deeper than the walk goes is one whose names are not all here
            self.complete = false;
            return;
        }
        if !self.visited.insert((
            file,
            binding.to_compact_string(),
            namespace.to_compact_string(),
        )) {
            return;
        }

        let db = self.db;
        let conf = urlconf(db, file);
        self.complete &= conf.complete;

        // including a module mounts its `urlpatterns`, and a route the module
        // writes outside any list at all is taken along rather than lost
        let whole_module = binding == URLPATTERNS_VARIABLE;
        let mut mounted_anything = false;

        for entry in &conf.entries {
            let is_mounted = match &entry.binding {
                Some(bound) => bound == binding,
                None => whole_module,
            };
            if !is_mounted {
                continue;
            }
            mounted_anything = true;

            match &entry.kind {
                UrlEntryKind::Route(route) => self.found.push(mounted(route, namespace, prefix)),
                UrlEntryKind::Include(include) => {
                    self.follow(file, include, namespace, prefix, depth);
                }
            }
        }

        // a list this module never binds is one built somewhere the scan cannot
        // read — by a helper, or by a router registering through a name of its
        // own — so what it mounts is not in the answer
        self.complete &= whole_module || mounted_anything;
    }

    /// walk what an include mounts, under the namespace it mounts it in
    fn follow(
        &mut self,
        file: File,
        include: &Include,
        namespace: &str,
        prefix: Option<&str>,
        depth: usize,
    ) {
        let prefix = join_routes(prefix, include.prefix.as_deref());

        match &include.target {
            IncludeTarget::Local(binding) => {
                let namespace = extend(namespace, include.namespace.as_deref());
                self.mount(file, binding, &namespace, prefix.as_deref(), depth - 1);
            }
            IncludeTarget::Module(module) => {
                let Some(included) = ModuleName::new(module)
                    .and_then(|module| resolve_real_module(self.db, file, &module))
                    .and_then(|module| module.file(self.db))
                else {
                    // a urlconf that can't be reached holds names the walk will
                    // never see
                    self.complete = false;
                    return;
                };
                // an include that names no namespace leaves the included module
                // to name its own
                let instance = include
                    .namespace
                    .clone()
                    .or_else(|| urlconf(self.db, included).app_name.clone());
                let namespace = extend(namespace, instance.as_deref());

                self.mount(
                    included,
                    URLPATTERNS_VARIABLE,
                    &namespace,
                    prefix.as_deref(),
                    depth - 1,
                );
            }
        }
    }
}

/// one route pattern written after another, when both are known
fn join_routes(prefix: Option<&str>, own: Option<&str>) -> Option<CompactString> {
    Some(format!("{}{}", prefix?, own?).to_compact_string())
}

/// `prefix` with `namespace` qualified under it, as django writes it
fn extend(prefix: &str, namespace: Option<&str>) -> CompactString {
    match namespace {
        Some(namespace) if prefix.is_empty() => namespace.to_compact_string(),
        Some(namespace) => format!("{prefix}{NAMESPACE_SEPARATOR}{namespace}").to_compact_string(),
        None => prefix.to_compact_string(),
    }
}

/// `route` as it is reversed from under `namespace`, behind the pattern `prefix`
fn mounted(route: &UrlName, namespace: &str, prefix: Option<&str>) -> UrlName {
    UrlName {
        name: extend(namespace, Some(&route.name)),
        route: join_routes(prefix, route.route.as_deref()).map(|route| route.as_str().into()),
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

/// where a python module writes a template or a route name
///
/// [`super::python`] reads one of these at a cursor, which is all a completion or
/// a goto ever needs. a rename needs the opposite: every place in the project the
/// name is written, since one left behind is a project silently broken. so an
/// expression in one of those positions is recorded whether or not it is a
/// literal — a `render(request, chosen)` names a template nothing here can read,
/// and what a rename cannot read it must refuse rather than work around.
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct NameUse {
    /// the literal's value, or `None` where the expression is no literal at all
    pub(crate) name: Option<CompactString>,
    /// whether the name is written as one literal a single range could replace
    ///
    /// `"blog/" "post.html"` reads as one name to python and as nothing a range
    /// could cover to a rename.
    pub(crate) whole: bool,
    pub(crate) file: File,
    /// the literal's contents, its quotes excluded — or the whole expression,
    /// where there is nothing to replace, so that a refusal can point at it
    pub(crate) range: TextRange,
}

impl NameUse {
    /// what `expr` writes, read as a name
    fn of(file: File, expr: &Expr) -> Self {
        let Expr::StringLiteral(literal) = expr else {
            return Self {
                name: None,
                whole: false,
                file,
                range: expr.range(),
            };
        };

        let (whole, range) = match literal.value.as_slice() {
            [part] => (true, part.content_range()),
            _ => (false, literal.range()),
        };

        Self {
            name: Some(literal.value.to_str().to_compact_string()),
            whole,
            file,
            range,
        }
    }
}

/// every place the project's python names a template
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn template_uses(db: &dyn Db, project: Project) -> Box<[NameUse]> {
    project_scan(db, project, |db, file| {
        template_uses_in_file(db, file).iter().cloned()
    })
}

/// every place the project's python reverses a route by name
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn route_uses(db: &dyn Db, project: Project) -> Box<[NameUse]> {
    project_scan(db, project, |db, file| {
        route_uses_in_file(db, file).iter().cloned()
    })
}

/// the templates one module names
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn template_uses_in_file(db: &dyn Db, file: File) -> Box<[NameUse]> {
    if !mentions(db, file, CONTEXT_CALLEES) && !mentions(db, file, &[TEMPLATE_NAME_ATTRIBUTE]) {
        return Box::default();
    }

    let parsed = parsed_module(db, file).load(db);
    let mut visitor = TemplateUseVisitor {
        db,
        file,
        found: Vec::new(),
    };
    visitor.visit_body(parsed.suite());

    visitor.found.into_boxed_slice()
}

/// whether `func` names something that is definitely not django's
///
/// a callee is matched by its last segment, so `shortcuts.render` and `render` are
/// one — and so is a local helper that happens to be called `render`. recording a
/// use for one of those is worse than missing a real one: a use whose name is not
/// a literal refuses the whole rename and names the offending line, so one
/// `def render(target, edit)` anywhere in a project takes template renaming away
/// from all of it.
///
/// so a callee whose definition is positively somewhere other than django is not
/// one of these functions. anything short of that — a name nothing binds, or an
/// import whose module could not be resolved, which lands back on the `import`
/// statement rather than on what it names — leaves the callee as the name says.
/// refusing a rename is the safe direction; silently rewriting half a project is
/// not, so only a *positive* answer excludes.
fn resolves_outside_django(db: &dyn Db, file: File, func: &Expr) -> bool {
    let definitions = definitions_of(db, file, func);
    !definitions.is_empty()
        && definitions.iter().all(|resolved| {
            resolved.definition().is_some_and(|definition| {
                // an import that is still an import is one nothing followed
                !matches!(
                    definition.kind(db),
                    DefinitionKind::Import(_)
                        | DefinitionKind::ImportFrom(_)
                        | DefinitionKind::ImportFromSubmodule(_)
                        | DefinitionKind::StarImport(_)
                ) && file_to_module(db, definition.file(db))
                    .is_none_or(|module| !is_djangos(db, module))
            })
        })
}

struct TemplateUseVisitor<'db> {
    db: &'db dyn Db,
    file: File,
    found: Vec<NameUse>,
}

impl<'ast> Visitor<'ast> for TemplateUseVisitor<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // `template_name = "…"` names a template where django reads one, which is
        // the body of a view class
        if let Stmt::ClassDef(class) = stmt {
            self.found.extend(
                class
                    .body
                    .iter()
                    .filter_map(|statement| class_attribute(statement, TEMPLATE_NAME_ATTRIBUTE))
                    .map(|(value, _)| NameUse::of(self.file, value)),
            );
        }

        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr
            && callee_name(&call.func)
                .is_some_and(|callee| CONTEXT_CALLEES.contains(&callee.as_str()))
            && let Some(template) = call
                .arguments
                .find_argument_value(TEMPLATE_NAME_ATTRIBUTE, 1)
            && !resolves_outside_django(self.db, self.file, &call.func)
        {
            self.found.push(NameUse::of(self.file, template));
        }

        walk_expr(self, expr);
    }
}

/// the routes one module reverses
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn route_uses_in_file(db: &dyn Db, file: File) -> Box<[NameUse]> {
    if !mentions(db, file, REVERSE_CALLEES)
        && !mentions(db, file, &[REDIRECT_CALLEE, HYPERLINK_NAME_KEYWORD])
    {
        return Box::default();
    }

    let parsed = parsed_module(db, file).load(db);
    let mut visitor = RouteUseVisitor {
        db,
        file,
        found: Vec::new(),
    };
    visitor.visit_body(parsed.suite());

    visitor.found.into_boxed_slice()
}

struct RouteUseVisitor<'db> {
    db: &'db dyn Db,
    file: File,
    found: Vec<NameUse>,
}

impl<'ast> Visitor<'ast> for RouteUseVisitor<'_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr
            && let Some(callee) = callee_name(&call.func)
            && !resolves_outside_django(self.db, self.file, &call.func)
        {
            if REVERSE_CALLEES.contains(&callee.as_str())
                && let Some(argument) = call.arguments.find_argument_value(REVERSE_NAME_KEYWORD, 0)
            {
                self.found.push(NameUse::of(self.file, argument));
            } else if callee == REDIRECT_CALLEE
                && let Some(argument) = call.arguments.args.first()
                && let use_ = NameUse::of(self.file, argument)
                // a redirect takes a url or a model as readily as a route name,
                // so only a literal that could be a name is read as one — a
                // `redirect(book)` is not a route this has failed to read
                && use_
                    .name
                    .as_ref()
                    .is_some_and(|name| !name.contains('/'))
            {
                self.found.push(use_);
            }
        }

        // a rest framework hyperlinked field names the route it points at by
        // keyword, wherever it is built
        if let Expr::Call(call) = expr
            && let Some(keyword) = call.arguments.find_keyword(HYPERLINK_NAME_KEYWORD)
        {
            self.found.push(NameUse::of(self.file, &keyword.value));
        }

        walk_expr(self, expr);
    }
}

/// a constant the project binds one of the names being renamed to
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundName {
    pub(crate) file: File,
    /// the literal, so that a caller can tell a binding it is already rewriting
    /// from one it is not
    pub(crate) value: TextRange,
    /// the name the literal is bound to, which is what a refusal points at
    pub(crate) bound_to: CompactString,
    /// where that name is written
    pub(crate) range: TextRange,
}

/// every module- or class-level name the project binds one of `names` to
///
/// a rename rewrites the positions it recognises, and a name written straight
/// into one of them is one it can follow. a constant is the way a name reaches
/// such a position without being written there — `TEMPLATE = "blog/base.html"`
/// and then a `render(request, TEMPLATE)`, or a helper somewhere this cannot
/// see at all — and following that is beyond anything here, so it refuses.
///
/// the search is deliberately not "every literal that spells the name": a
/// `detail` or a `content` is among the commonest strings in any codebase, and
/// an `item.get("detail")` in unrelated code is no reason to refuse anything. a
/// binding is the narrowest thing that could still carry the name somewhere.
pub(crate) fn bound_names(db: &dyn Db, names: &[&str]) -> Vec<BoundName> {
    let mut found = Vec::new();

    for file in &db.project().files(db) {
        if is_stub(db, file) || !mentions(db, file, names) {
            continue;
        }

        let parsed = parsed_module(db, file).load(db);
        bindings_in(file, parsed.suite(), names, &mut found);
    }

    found
}

/// the bindings of `body`, and of the classes it declares
///
/// a function body is left out: a name bound there is visible only inside it, so
/// it can only reach a template or a route through a call this already reads —
/// and reads as an argument it cannot follow, which refuses on its own.
fn bindings_in(file: File, body: &[Stmt], names: &[&str], found: &mut Vec<BoundName>) {
    for statement in body {
        if let Stmt::ClassDef(class) = statement {
            bindings_in(file, &class.body, names, found);
            continue;
        }

        let Some((bound_to, range, value)) = binding(statement) else {
            continue;
        };
        let Expr::StringLiteral(literal) = value else {
            continue;
        };
        if !names.contains(&literal.value.to_str()) {
            continue;
        }

        found.push(BoundName {
            file,
            value: match literal.value.as_slice() {
                [part] => part.content_range(),
                _ => literal.range(),
            },
            bound_to: bound_to.to_compact_string(),
            range,
        });
    }
}

/// the name a statement binds, where it writes it, and what it binds to it
fn binding(statement: &Stmt) -> Option<(&str, TextRange, &Expr)> {
    match statement {
        Stmt::Assign(assign) => match assign.targets.as_slice() {
            [Expr::Name(target)] => Some((target.id.as_str(), target.range(), &*assign.value)),
            _ => None,
        },
        Stmt::AnnAssign(assign) => match (&*assign.target, assign.value.as_deref()) {
            (Expr::Name(target), Some(value)) => Some((target.id.as_str(), target.range(), value)),
            _ => None,
        },
        _ => None,
    }
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
        collect_under(db, project, root, precedence, true, &mut found);
    }

    // an installed app is as often as not a package in site-packages, whose
    // `templates` directory the convention walk of the project root never
    // reaches. django loads `admin/base_site.html` and `rest_framework/api.html`
    // from exactly there
    for (index, app) in order.apps.iter().enumerate() {
        collect_under(
            db,
            project,
            &app.join(directory),
            order.named.len() + index,
            false,
            &mut found,
        );
    }

    found.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.precedence.cmp(&right.precedence))
            .then(left.path.cmp(&right.path))
    });
    // one file reached two ways is one file, and the project's own if either way
    // was the project's
    found.dedup_by(|left, right| {
        let same = left.name == right.name && left.path == right.path;
        if same {
            right.own |= left.own;
        }
        same
    });
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
                                // the walk starts at the project root and
                                // respects its ignore rules, so whatever it
                                // reaches is the project's own
                                own: true,
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
    own: bool,
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
                        own,
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
            always_loaded: false,
        })
    }
}

impl<'ast> Visitor<'ast> for RegistrationVisitor {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(function) = stmt {
            let documentation = docstring_summary(&function.body);
            // one function may be registered under several names: django's own
            // `{% translate %}` and `{% trans %}` are one function carrying two
            // `@register.tag` decorators, and taking only the first loses the
            // older spelling entirely
            let registrations: Vec<Registration> = function
                .decorator_list
                .iter()
                .filter_map(|decorator| {
                    self.registration(decorator, &function.name, documentation.clone())
                })
                .collect();
            self.found.extend(registrations);
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
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
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
    /// the functions `TEMPLATES[*]["OPTIONS"]["context_processors"]` names, whose
    /// returned names every template is rendered with
    context_processors: Box<[CompactString]>,
    /// whether every directory and module the settings name was worked out
    ///
    /// a `DIRS` entry built from an environment variable, or an engine that
    /// configures its own `loaders`, leaves django loading from somewhere this
    /// has no way to look — which is a template set nothing may call complete.
    read_in_full: bool,
}

impl Default for DjangoSettings {
    fn default() -> Self {
        Self {
            template_dirs: Box::default(),
            app_directories: false,
            static_dirs: Box::default(),
            installed_apps: Box::default(),
            root_urlconf: None,
            always_loaded: Box::default(),
            context_processors: Box::default(),
            // the settings that were read, all none of them, were read in full
            read_in_full: true,
        }
    }
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
pub(crate) fn settings_file(db: &dyn Db, project: Project) -> Option<File> {
    settings_source::settings_file(db, settings_naming(db, project))
}

/// the project's `manage.py`, django's own entry point
///
/// this is the script `manage.py test`, `manage.py migrate` and the rest are run
/// through, so a project without one is a project in which none of them can be
/// run. it is identified the way [`settings_file`] identifies it — by naming
/// `DJANGO_SETTINGS_MODULE` — since a script that doesn't is no django entry
/// point whatever it happens to be called.
#[salsa::tracked]
pub(crate) fn manage_file(db: &dyn Db, project: Project) -> Option<File> {
    settings_source::entry_point_file(db, settings_naming(db, project))
}

/// every file of the project that names its settings module, in path order
///
/// the project's files come in no order worth relying on, and both consumers
/// have to land on the same file twice running.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn settings_naming(db: &dyn Db, project: Project) -> Box<[SettingsNaming]> {
    settings_source::settings_namings(db, &project.files(db)).into_boxed_slice()
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
                    let named = elements(&assign.value);
                    installed_apps.extend(named.iter().filter_map(string_literal));
                    // an app whose entry isn't written out is an app whose
                    // templates and libraries are not in the answer
                    self.settings.read_in_full &= installed_apps.len() == named.len();
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
    /// `builtins` is one: a module it names is loaded into every template the
    /// engine renders, so nothing in a template has to `{% load %}` it.
    /// `context_processors` is the other, and names the functions whose result
    /// every template is rendered with.
    fn options(&mut self, expr: &Expr) {
        let Expr::Dict(options) = expr else {
            return;
        };

        let mut always_loaded: Vec<CompactString> = self.settings.always_loaded.to_vec();
        let mut context_processors: Vec<CompactString> = self.settings.context_processors.to_vec();

        for item in &options.items {
            match item.key.as_ref().and_then(string_literal).as_deref() {
                Some(BUILTINS_OPTION) => {
                    let named = elements(&item.value);
                    always_loaded.extend(named.iter().filter_map(string_literal));
                    self.settings.read_in_full &= always_loaded.len() == named.len();
                }
                // a processor that can't be read costs a name nothing reports
                // against, so it leaves the settings authoritative as they were
                Some(CONTEXT_PROCESSORS_OPTION) => {
                    context_processors
                        .extend(elements(&item.value).iter().filter_map(string_literal));
                }
                // an engine listing its own loaders loads from wherever they say,
                // which need not be a directory at all
                Some(LOADERS_OPTION) => self.settings.read_in_full = false,
                _ => {}
            }
        }

        self.settings.always_loaded = always_loaded.into_boxed_slice();
        self.settings.context_processors = context_processors.into_boxed_slice();
    }

    /// the directories a list of them names, the ones that can't be worked out dropped
    fn directories(&mut self, expr: &Expr) -> Vec<SystemPathBuf> {
        let named = elements(expr);
        let found: Vec<SystemPathBuf> = named
            .iter()
            .filter_map(|element| self.paths.path(element))
            .collect();

        // a directory django loads from and this cannot name is one whose
        // templates are missing from every answer below
        self.settings.read_in_full &= found.len() == named.len();
        found
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
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
struct UrlConf {
    /// the namespace an include of this module falls back to
    app_name: Option<CompactString>,
    entries: Box<[UrlEntry]>,
    /// whether every route the module names was read
    ///
    /// a route whose name or whose mounted list could not be worked out leaves a
    /// name behind that nothing here will ever report, which is exactly the
    /// state in which a "no such route" would be wrong.
    complete: bool,
}

impl Default for UrlConf {
    fn default() -> Self {
        Self {
            app_name: None,
            entries: Box::default(),
            // a module that names no route at all is one there was nothing to miss in
            complete: true,
        }
    }
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
    /// the route pattern the include is mounted behind, within this module
    prefix: Option<CompactString>,
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
        prefix: Some(CompactString::default()),
        found: Vec::new(),
        complete: true,
    };
    visitor.visit_body(parsed.suite());

    UrlConf {
        app_name,
        entries: visitor.found.into_boxed_slice(),
        complete: visitor.complete,
    }
}

struct UrlVisitor<'db> {
    db: &'db dyn Db,
    file: File,
    /// the module-level name the statement being visited binds
    binding: Option<CompactString>,
    /// the route pattern the call being visited is written inside, or `None`
    /// where one of the patterns enclosing it could not be read
    prefix: Option<CompactString>,
    found: Vec<UrlEntry>,
    /// whether every route this module names was read
    complete: bool,
}

impl UrlVisitor<'_> {
    /// record `route` as a route of the list `binding`
    fn record(&mut self, binding: Option<CompactString>, route: UrlName) {
        self.found.push(UrlEntry {
            binding,
            kind: UrlEntryKind::Route(route),
        });
    }

    /// record that the list being bound mounts `target` under `namespace`
    fn mounts(&mut self, target: IncludeTarget, namespace: Option<CompactString>) {
        self.found.push(UrlEntry {
            binding: self.binding.clone(),
            kind: UrlEntryKind::Include(Include {
                target,
                namespace,
                prefix: self.prefix.clone(),
            }),
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
                _ => {
                    self.complete = false;
                    return;
                }
            },
            _ => (argument, None),
        };
        let Some(target) = include_target(target) else {
            // whatever this mounts, its names are not in the answer
            self.complete = false;
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
            UrlName {
                name,
                file: self.file,
                range: call.func.range(),
                route: None,
                exact: false,
                view: None,
                extra_arguments: false,
            },
        );
    }

    /// the name a `path()`-like call gives its route
    fn path_call(&mut self, call: &ast::ExprCall) {
        if !is_url_call(call) {
            return;
        }
        let Some(keyword) = call.arguments.find_keyword(URL_NAME_KEYWORD) else {
            return;
        };
        let Some(name) = string_literal(&keyword.value) else {
            // a route django names and this doesn't is one a "no such route"
            // would be wrong about
            self.complete = false;
            return;
        };

        let route = join_routes(self.prefix.as_deref(), route_of(call).as_deref());
        let view = call.arguments.args.get(1).and_then(|argument| {
            let (callable, class_based) = view_callable(argument);

            Some(RouteView {
                target: resolved_target(self.db, self.file, callable)?,
                range: callable.range(),
                class_based,
            })
        });
        // `path(route, view, kwargs, name)`: whatever the third argument holds is
        // passed to the view alongside what the pattern captured
        let extra_arguments =
            call.arguments.args.len() > 2 || call.arguments.find_keyword("kwargs").is_some();

        self.record(
            self.binding.clone(),
            UrlName {
                name,
                file: self.file,
                range: keyword.value.range(),
                route: route.map(|route| route.as_str().into()),
                exact: true,
                view,
                extra_arguments,
            },
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
            self.complete = false;
            return;
        };
        // the viewset is named either directly or through the module it lives in
        let Some(viewset @ (Expr::Name(_) | Expr::Attribute(_))) =
            call.arguments.find_argument_value("viewset", 1)
        else {
            self.complete = false;
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
                None => {
                    self.complete = false;
                    return;
                }
            },
        };

        // a router writes its own patterns from the prefix rather than taking
        // them, so the prefix is what a route reads as but not what it takes
        let route = join_routes(self.prefix.as_deref(), Some(prefix.as_str()));
        // a router hands each generated route `viewset.as_view({…})`, so what
        // serves the request is one of the viewset's own methods
        let view = resolved_target(self.db, self.file, viewset).map(|target| RouteView {
            target,
            range: viewset.range(),
            class_based: true,
        });
        let generated = |name: String, file: File, range: TextRange| UrlName {
            name: name.to_compact_string(),
            file,
            range,
            route: route.clone().map(|route| route.as_str().into()),
            // a router writes its own patterns rather than taking them
            exact: false,
            view: view.clone(),
            extra_arguments: false,
        };

        for suffix in ROUTER_ROUTE_SUFFIXES {
            self.record(
                binding.clone(),
                generated(format!("{basename}-{suffix}"), self.file, anchor),
            );
        }

        for action in described.iter().flat_map(|described| &described.actions) {
            // an action's route is that method's, so that is where it leads
            self.record(
                binding.clone(),
                generated(
                    format!("{basename}-{}", action.url_name),
                    action.file,
                    action.range,
                ),
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
pub(crate) fn resolved_class<T>(
    db: &dyn Db,
    file: File,
    expr: &Expr,
    mut read: impl FnMut(File, &ast::StmtClassDef) -> T,
) -> Option<T> {
    definitions_of(db, file, expr)
        .into_iter()
        .find_map(|resolved| {
            let definition = resolved.definition()?;
            let defining = definition.file(db);
            let parsed = parsed_module(db, defining).load(db);
            let class = definition.kind(db).as_class()?.node(&parsed);

            Some(read(defining, class))
        })
}

/// every definition `expr` resolves to, import aliases followed to their source
fn definitions_of<'db>(db: &'db dyn Db, file: File, expr: &Expr) -> Vec<ResolvedDefinition<'db>> {
    let model = SemanticModel::new(db, file);

    match expr {
        Expr::Name(name) => definitions_for_name(
            &model,
            name.id.as_str(),
            AnyNodeRef::from(name),
            ImportAliasResolution::ResolveAliases,
        ),
        Expr::Attribute(attribute) => definitions_for_attribute(&model, attribute),
        _ => Vec::new(),
    }
}

/// the class or function `expr` names, wherever it is declared
fn resolved_target(db: &dyn Db, file: File, expr: &Expr) -> Option<Target> {
    definitions_of(db, file, expr)
        .into_iter()
        .find_map(|resolved| {
            let definition = resolved.definition()?;
            let defining = definition.file(db);
            let parsed = parsed_module(db, defining).load(db);

            let (name, kind, full_range) = match definition.kind(db) {
                kind if kind.as_class().is_some() => {
                    let class = kind.as_class()?.node(&parsed);
                    (&class.name, TargetKind::Class, class.range())
                }
                kind => {
                    let function = kind.as_function()?.node(&parsed);
                    (&function.name, TargetKind::Function, function.range())
                }
            };

            Some(Target {
                name: name.id.to_compact_string(),
                kind,
                file: defining,
                range: name.range(),
                full_range,
            })
        })
}

/// what a route's view argument points at, and whether it was reached through
/// `as_view()`
///
/// a class-based view is written into a route through `as_view()`, and what the
/// route reaches is the class that method belongs to. that a class was reached
/// that way is worth keeping: it is the difference between a class django calls
/// through its handler methods and one a route hands the request to directly.
fn view_callable(expr: &Expr) -> (&Expr, bool) {
    match expr {
        Expr::Call(call) => match &*call.func {
            Expr::Attribute(attribute) if attribute.attr.as_str() == AS_VIEW_METHOD => {
                (&attribute.value, true)
            }
            _ => (expr, false),
        },
        _ => (expr, false),
    }
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
        let Expr::Call(call) = expr else {
            walk_expr(self, expr);
            return;
        };

        self.path_call(call);
        self.router_registration(call);
        self.include_call(call);

        // whatever this call holds — an `include()`, or a list of further routes —
        // django mounts behind this call's own pattern
        let outer = is_url_call(call).then(|| {
            let inner = join_routes(self.prefix.as_deref(), route_of(call).as_deref());
            std::mem::replace(&mut self.prefix, inner)
        });

        walk_expr(self, expr);

        if let Some(outer) = outer {
            self.prefix = outer;
        }
    }
}

/// whether `call` is a `path()`, `re_path()` or `url()`
fn is_url_call(call: &ast::ExprCall) -> bool {
    callee_name(&call.func).is_some_and(|callee| URL_CALLEES.contains(&callee.as_str()))
}

/// the pattern a `path()`-like call matches, when it is written out
fn route_of(call: &ast::ExprCall) -> Option<CompactString> {
    string_literal(call.arguments.args.first()?)
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
        db,
        file,
        found: Vec::new(),
        scopes: Vec::new(),
    };
    visitor.visit_body(parsed.suite());

    visitor.found.into_boxed_slice()
}

struct ContextVisitor<'db, 'ast> {
    db: &'db dyn Db,
    file: File,
    found: Vec<TemplateContext>,
    /// the functions being walked, innermost last
    ///
    /// a context handed to `render()` by name is built by the statements of the
    /// function that binds it, so that body is what the name is followed in —
    /// and the same function is the view a lens names.
    scopes: Vec<&'ast ast::StmtFunctionDef>,
}

impl<'ast> ContextVisitor<'_, 'ast> {
    /// the view a render call found here is written in
    fn enclosing_view(&self) -> Option<ViewRef> {
        let innermost = self.scopes.last()?;

        let mut path = CompactString::default();
        for function in &self.scopes {
            if !path.is_empty() {
                path.push('.');
            }
            path.push_str(function.name.as_str());
        }

        Some(ViewRef {
            path,
            file: self.file,
            range: innermost.name.range(),
        })
    }

    /// the context a `render()`/`TemplateResponse()` call passes
    ///
    /// both take the request first, the template second and the context third,
    /// and both accept those last two by keyword as well.
    fn render_call(&self, call: &'ast ast::ExprCall) -> Option<TemplateContext> {
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
            .find_keyword(CONTEXT_KEYWORD)
            .map(|keyword| &keyword.value)
            .or_else(|| call.arguments.args.get(context_index));

        Some(TemplateContext {
            template,
            view: self.enclosing_view(),
            variables: context
                .map(|context| {
                    context_variables(
                        self.file,
                        self.scopes.last().map(|function| function.body.as_slice()),
                        context,
                        MAX_CONTEXT_DEPTH,
                    )
                })
                .unwrap_or_default()
                .into_boxed_slice(),
        })
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
        let mut seen = Vec::new();
        view_context(
            self.db,
            self.file,
            class,
            &mut variables,
            &mut seen,
            MAX_VIEW_DEPTH,
        );

        Some(TemplateContext {
            template,
            view: Some(ViewRef {
                path: class.name.id.to_compact_string(),
                file: self.file,
                range: class.name.range(),
            }),
            variables: variables.into_boxed_slice(),
        })
    }
}

impl<'ast> Visitor<'ast> for ContextVisitor<'_, 'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::ClassDef(class) => self.found.extend(self.class_based_view(class)),
            Stmt::FunctionDef(function) => {
                self.scopes.push(function);
                walk_stmt(self, stmt);
                self.scopes.pop();
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            self.found.extend(self.render_call(call));
        }

        walk_expr(self, expr);
    }
}

/// everything a view class and the classes it inherits from put in the context
///
/// a base's `get_context_data` reaches every template its subclasses name, so the
/// chain is walked. a class the walk has already read is not read again, and the
/// walk is bounded: neither an inheritance chain read off the ast nor the
/// resolution that follows it promises to be finite.
fn view_context(
    db: &dyn Db,
    file: File,
    class: &ast::StmtClassDef,
    found: &mut Vec<ContextVariable>,
    seen: &mut Vec<(File, TextRange)>,
    depth: usize,
) {
    if depth == 0 || seen.contains(&(file, class.range())) {
        return;
    }
    seen.push((file, class.range()));

    declared_context(file, class, found);

    for base in class.bases() {
        resolved_class(db, file, base, |defining, base_class| {
            view_context(db, defining, base_class, found, seen, depth - 1);
        });
    }
}

/// the names one view class's own body declares
///
/// a name the class declares itself outranks the same name from a base, exactly
/// as it does at render time, so what is already found stays.
fn declared_context(file: File, class: &ast::StmtClassDef, found: &mut Vec<ContextVariable>) {
    for statement in &class.body {
        if let Some((value, range)) = class_attribute(statement, CONTEXT_OBJECT_NAME_ATTRIBUTE)
            && let Some(name) = string_literal(value)
        {
            merge_unseen(
                found,
                [ContextVariable {
                    name,
                    file,
                    range,
                    // the object itself is what the view will bind; its type
                    // comes from the view's generics, which is beyond what a
                    // syntactic scan can follow
                    value: None,
                    source: ContextSource::View,
                }],
            );
        }

        if let Some((value, _)) = class_attribute(statement, EXTRA_CONTEXT_ATTRIBUTE) {
            merge_unseen(
                found,
                context_variables(file, None, value, MAX_CONTEXT_DEPTH),
            );
        }

        if let Stmt::FunctionDef(function) = statement
            && function.name.as_str() == GET_CONTEXT_DATA_METHOD
        {
            merge_unseen(found, returned_context(file, &function.body));
        }
    }
}

/// the names the dict a `get_context_data` body returns holds
///
/// what it returns is read first, and every `context["name"] = …` of the body is
/// taken on top of that: a body that hands its dict on to `super()` rather than
/// returning it still writes the names it writes.
fn returned_context(file: File, body: &[Stmt]) -> Vec<ContextVariable> {
    let mut returns = ReturnVisitor { found: Vec::new() };
    returns.visit_body(body);

    let mut found = Vec::new();
    for value in returns.found {
        merge_unseen(
            &mut found,
            context_variables(file, Some(body), value, MAX_CONTEXT_DEPTH),
        );
    }

    let mut writes = Vec::new();
    let mut visitor = ContextAssignmentVisitor {
        file,
        found: &mut writes,
    };
    visitor.visit_body(body);

    merge_unseen(&mut found, writes);
    found
}

/// the names `expr` contributes to a context
///
/// a dict literal binds its keys. a name is followed to what the body that binds
/// it holds there, which is the whole of the difference between a context written
/// out in the `render()` call and the far commoner one built up above it. anything
/// else — a call, a comprehension, a name with no body to look in — contributes
/// nothing rather than a guess.
fn context_variables(
    file: File,
    scope: Option<&[Stmt]>,
    expr: &Expr,
    fuel: usize,
) -> Vec<ContextVariable> {
    let mut found = Vec::new();

    match expr {
        Expr::Dict(dict) => {
            for item in &dict.items {
                match item.key.as_ref() {
                    Some(key) => {
                        let Some(name) = string_literal(key) else {
                            continue;
                        };
                        merge_over(
                            &mut found,
                            ContextVariable {
                                name,
                                file,
                                range: key.range(),
                                value: Some(item.value.range()),
                                source: ContextSource::View,
                            },
                        );
                    }
                    // `{**base, "extra": …}` spreads whatever `base` holds here
                    None if fuel > 0 => {
                        for variable in context_variables(file, scope, &item.value, fuel - 1) {
                            merge_over(&mut found, variable);
                        }
                    }
                    None => {}
                }
            }
        }
        Expr::Name(name) if fuel > 0 => {
            if let Some(scope) = scope {
                found = bound_context(file, scope, name.id.as_str(), name.start(), fuel - 1);
            }
        }
        _ => {}
    }

    found
}

/// what the local `name` holds by the time execution reaches `before`
///
/// the statements that build it are read in the order they are written: a
/// rebinding replaces what came before it, a subscript write and an `update()`
/// add to it. that is the dict `render()` is handed.
fn bound_context(
    file: File,
    scope: &[Stmt],
    name: &str,
    before: TextSize,
    fuel: usize,
) -> Vec<ContextVariable> {
    let mut mutations = MutationVisitor {
        name,
        before,
        found: Vec::new(),
    };
    mutations.visit_body(scope);

    let mut found = Vec::new();
    for mutation in mutations.found {
        match mutation {
            Mutation::Bound(value) => {
                found = context_variables(file, Some(scope), value, fuel);
            }
            Mutation::Extended(value) => {
                for variable in context_variables(file, Some(scope), value, fuel) {
                    merge_over(&mut found, variable);
                }
            }
            Mutation::Wrote(key, value) => {
                let Some(name) = string_literal(key) else {
                    continue;
                };
                merge_over(
                    &mut found,
                    ContextVariable {
                        name,
                        file,
                        range: key.range(),
                        value: Some(value.range()),
                        source: ContextSource::View,
                    },
                );
            }
        }
    }

    found
}

/// add `variable`, replacing a name already there
///
/// a dict written twice over holds what was written last, and so does the
/// context django renders with.
fn merge_over(found: &mut Vec<ContextVariable>, variable: ContextVariable) {
    match found
        .iter_mut()
        .find(|existing| existing.name == variable.name)
    {
        Some(existing) => *existing = variable,
        None => found.push(variable),
    }
}

/// add every name of `extra` that isn't spoken for already
///
/// where two independent sources name the same thing it is the first that wins,
/// which is what puts a view's own declaration above the one it inherits.
fn merge_unseen(
    found: &mut Vec<ContextVariable>,
    extra: impl IntoIterator<Item = ContextVariable>,
) {
    for variable in extra {
        if !found.iter().any(|existing| existing.name == variable.name) {
            found.push(variable);
        }
    }
}

/// one statement that builds up a context held in a variable
enum Mutation<'ast> {
    /// `context = value`, which replaces whatever it held
    Bound(&'ast Expr),
    /// `context.update(value)`
    Extended(&'ast Expr),
    /// `context["name"] = value`
    Wrote(&'ast Expr, &'ast Expr),
}

/// collects, in the order written, what one function body does to one name
struct MutationVisitor<'a, 'ast> {
    name: &'a str,
    /// where the context is read, past which nothing has run yet
    before: TextSize,
    found: Vec<Mutation<'ast>>,
}

impl MutationVisitor<'_, '_> {
    /// whether `expr` is the name being followed
    fn is_target(&self, expr: &Expr) -> bool {
        matches!(expr, Expr::Name(name) if name.id.as_str() == self.name)
    }
}

impl<'ast> Visitor<'ast> for MutationVisitor<'_, 'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // a statement below the read has not run when the context is handed over,
        // and one in a body of its own belongs to that body rather than to this
        if stmt.start() >= self.before || matches!(stmt, Stmt::FunctionDef(_) | Stmt::ClassDef(_)) {
            return;
        }

        match stmt {
            Stmt::Assign(assign) => match assign.targets.as_slice() {
                [target] if self.is_target(target) => {
                    self.found.push(Mutation::Bound(&assign.value));
                }
                [Expr::Subscript(subscript)] if self.is_target(&subscript.value) => {
                    self.found
                        .push(Mutation::Wrote(&subscript.slice, &assign.value));
                }
                _ => {}
            },
            Stmt::AnnAssign(assign) => {
                if self.is_target(&assign.target)
                    && let Some(value) = assign.value.as_deref()
                {
                    self.found.push(Mutation::Bound(value));
                }
            }
            Stmt::Expr(expression) => {
                if let Expr::Call(call) = &*expression.value
                    && let Expr::Attribute(attribute) = &*call.func
                    && attribute.attr.as_str() == CONTEXT_UPDATE_METHOD
                    && self.is_target(&attribute.value)
                    && let [argument] = call.arguments.args.as_ref()
                {
                    self.found.push(Mutation::Extended(argument));
                }
            }
            _ => walk_stmt(self, stmt),
        }
    }
}

/// collects the value of every `return` of a body, its nested bodies apart
struct ReturnVisitor<'ast> {
    found: Vec<&'ast Expr>,
}

impl<'ast> Visitor<'ast> for ReturnVisitor<'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            // a nested function returns for itself, not for the body holding it
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            Stmt::Return(statement) => self.found.extend(statement.value.as_deref()),
            _ => walk_stmt(self, stmt),
        }
    }
}

/// collects `context["name"] = value` writes
///
/// the dict written into has to be a name of the body's own: a write to
/// `request.session["cart"]` is a write to something that is not a context at
/// all, and reading it as one would put a name in every template that has none.
struct ContextAssignmentVisitor<'a> {
    file: File,
    found: &'a mut Vec<ContextVariable>,
}

impl<'ast> Visitor<'ast> for ContextAssignmentVisitor<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::Assign(assign) = stmt
            && let [Expr::Subscript(subscript)] = assign.targets.as_slice()
            && matches!(&*subscript.value, Expr::Name(_))
            && let Some(name) = string_literal(&subscript.slice)
        {
            self.found.push(ContextVariable {
                name,
                file: self.file,
                range: subscript.slice.range(),
                value: Some(assign.value.range()),
                source: ContextSource::View,
            });
        }

        walk_stmt(self, stmt);
    }
}

/// every name a context processor puts in every template's context
///
/// `TEMPLATES[*]["OPTIONS"]["context_processors"]` names functions django calls
/// on each render and merges the dict of into the context, so `request` and
/// `user` are written in templates no view ever mentions them to.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn context_processor_variables(db: &dyn Db, project: Project) -> Box<[ContextVariable]> {
    let Some(importing) = *settings_file(db, project) else {
        return Box::default();
    };

    // django merges each processor's dict over the ones before it, so where two
    // of them name the same thing it is the last that renders
    let mut found = Vec::new();
    for processor in &django_settings(db, project).context_processors {
        for variable in processor_variables(db, importing, processor) {
            merge_over(&mut found, variable);
        }
    }

    found.into_boxed_slice()
}

/// the names the processor the dotted `name` resolves to returns
///
/// only what a `return` of the function itself can be read as a dict is
/// answered. a processor building its dict some other way — a comprehension, a
/// call into something else — contributes nothing rather than a guessed name.
fn processor_variables(db: &dyn Db, importing: File, name: &str) -> Vec<ContextVariable> {
    let Some((module, function)) = name.rsplit_once('.') else {
        return Vec::new();
    };
    // what a processor returns is only ever written in the module that runs
    let Some(file) = ModuleName::new(module)
        .and_then(|module| resolve_real_module(db, importing, &module))
        .and_then(|module| module.file(db))
    else {
        return Vec::new();
    };

    let parsed = parsed_module(db, file).load(db);
    let Some(definition) = parsed.suite().iter().find_map(|statement| match statement {
        Stmt::FunctionDef(definition) if definition.name.as_str() == function => Some(definition),
        _ => None,
    }) else {
        return Vec::new();
    };

    let mut found = returned_context(file, &definition.body);
    for variable in &mut found {
        variable.source = ContextSource::Processor;
    }
    found
}

/// what django does with a class the project declares
#[derive(Debug, Clone, Copy, PartialEq, Eq, get_size2::GetSize)]
pub(crate) enum DjangoClassKind {
    /// a subclass of django's `Model`, which is a table
    Model,
    /// a subclass of django's `ModelAdmin`, which is a model's admin page
    Admin,
}

/// where a class is declared, which is what identifies it across files
///
/// its name is not enough — two apps may each hold a `BookAdmin` — and the
/// class name's own range is unique within the file that declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(crate) struct ClassRef {
    pub(crate) file: File,
    pub(crate) range: TextRange,
}

/// a class django gives a role to
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct DjangoClass {
    pub(crate) name: CompactString,
    pub(crate) kind: DjangoClassKind,
    pub(crate) file: File,
    /// the class name, for navigation
    pub(crate) range: TextRange,
    /// the whole `class …:` statement
    pub(crate) full_range: TextRange,
    /// the classes it is written against, wherever they are declared
    ///
    /// this is what tells a base others are built on from a leaf nothing uses.
    pub(crate) bases: Box<[ClassRef]>,
}

/// every model and admin class the project declares
///
/// unlike the scans above, this one is not gated on the source spelling any
/// particular name: a model need not write django's own base itself, only
/// inherit from something that eventually does, so there is no identifier such a
/// class has to hold. what keeps it affordable is that a class with no bases
/// costs nothing at all, and that each file's bases are followed once, in a
/// query salsa keeps.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn django_classes(db: &dyn Db, project: Project) -> Box<[DjangoClass]> {
    project_scan(db, project, |db, file| {
        django_classes_in_file(db, file).iter().cloned()
    })
}

/// the models and admin classes one module declares
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn django_classes_in_file(db: &dyn Db, file: File) -> Box<[DjangoClass]> {
    // a class only classifies by reaching a base django itself declares, and a
    // module of django's is reached through an import that spells the package.
    // so a project no file of which writes it has no such class anywhere, and
    // this is what keeps a project that is no django project from having its
    // every class followed up its bases
    if !project_uses_django(db, db.project()) {
        return Box::default();
    }

    let parsed = parsed_module(db, file).load(db);
    let mut visitor = DjangoClassVisitor {
        db,
        file,
        found: Vec::new(),
    };
    visitor.visit_body(parsed.suite());

    visitor.found.into_boxed_slice()
}

struct DjangoClassVisitor<'db> {
    db: &'db dyn Db,
    file: File,
    found: Vec<DjangoClass>,
}

impl<'ast> Visitor<'ast> for DjangoClassVisitor<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::ClassDef(class) = stmt
            && let Some(kind) = django_class_kind(self.db, self.file, class, MAX_BASE_DEPTH)
        {
            self.found.push(DjangoClass {
                name: class.name.id.to_compact_string(),
                kind,
                file: self.file,
                range: class.name.range(),
                full_range: class.range(),
                bases: class
                    .bases()
                    .iter()
                    .filter_map(|base| class_ref(self.db, self.file, base))
                    .collect(),
            });
        }

        walk_stmt(self, stmt);
    }
}

/// whether anything in the project so much as names django
///
/// the two halves are separate queries so that an edit to one module re-reads
/// that module's source and no other, and so that the answer — which changes
/// about once in a project's life — is backdated rather than invalidating every
/// scan that reads it.
#[salsa::tracked(returns(copy))]
fn project_uses_django(db: &dyn Db, project: Project) -> bool {
    project
        .files(db)
        .iter()
        .any(|file| file_names_django(db, *file))
}

#[salsa::tracked(returns(copy))]
fn file_names_django(db: &dyn Db, file: File) -> bool {
    mentions(db, file, &[DJANGO_PACKAGE])
}

/// the role django gives `class`, from the bases it is written against
///
/// the walk goes up the bases rather than over the name the source writes, so a
/// class three subclasses removed from django's own — or one that imported it
/// under another name — is classified exactly as a direct subclass is. a base
/// that cannot be resolved to a class contributes nothing, which leaves the
/// class unclassified rather than guessed at.
fn django_class_kind(
    db: &dyn Db,
    file: File,
    class: &ast::StmtClassDef,
    depth: usize,
) -> Option<DjangoClassKind> {
    if depth == 0 {
        return None;
    }

    class.bases().iter().find_map(|base| {
        resolved_class(db, file, base, |defining, resolved| {
            if is_djangos_own(db, defining) {
                match resolved.name.as_str() {
                    MODEL_BASE => return Some(DjangoClassKind::Model),
                    MODEL_ADMIN_BASE => return Some(DjangoClassKind::Admin),
                    _ => {}
                }
            }

            django_class_kind(db, defining, resolved, depth - 1)
        })
        .flatten()
    })
}

/// whether `class` is one django's test runner would collect
///
/// the walk goes up the bases towards `unittest.TestCase` rather than towards
/// any of django's own bases, because that is the one class every test case
/// reaches: django's `SimpleTestCase` is written against it, `TestCase` is
/// written against that, and a project's own base is written against those. a
/// plain `unittest.TestCase` in a django project is collected by the runner just
/// the same, so reaching it is the whole of the question.
pub(crate) fn is_test_class(db: &dyn Db, file: File, class: &ast::StmtClassDef) -> bool {
    fn walk(db: &dyn Db, file: File, class: &ast::StmtClassDef, depth: usize) -> bool {
        if depth == 0 {
            return false;
        }

        class.bases().iter().any(|base| {
            resolved_class(db, file, base, |defining, resolved| {
                (resolved.name.as_str() == TEST_CASE_BASE && is_unittests_own(db, defining))
                    || walk(db, defining, resolved, depth - 1)
            })
            .unwrap_or_default()
        })
    }

    walk(db, file, class, MAX_BASE_DEPTH)
}

/// whether `file` is one of `unittest`'s own modules
fn is_unittests_own(db: &dyn Db, file: File) -> bool {
    file_to_module(db, file)
        .is_some_and(|module| module.name(db).components().next() == Some(UNITTEST_PACKAGE))
}

/// where the class `expr` names is declared
fn class_ref(db: &dyn Db, file: File, expr: &Expr) -> Option<ClassRef> {
    resolved_class(db, file, expr, |defining, class| ClassRef {
        file: defining,
        range: class.name.range(),
    })
}

/// whether `file` is one of django's own modules
fn is_djangos_own(db: &dyn Db, file: File) -> bool {
    file_to_module(db, file).is_some_and(|module| is_djangos(db, module))
}

/// what the admin registrations of one scope say
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct AdminRegistrations {
    /// the admin classes registered
    pub(crate) registered: Box<[ClassRef]>,
    /// whether every registration was read in full
    ///
    /// a registration whose model or whose admin class could not be worked out
    /// leaves a class registered that nothing here will ever match, which is
    /// exactly the state in which a "nothing registers this" would be wrong.
    pub(crate) complete: bool,
}

impl Default for AdminRegistrations {
    fn default() -> Self {
        Self {
            registered: Box::default(),
            // a module that registers nothing is one there was nothing to miss in
            complete: true,
        }
    }
}

/// every admin class the project registers
///
/// nothing reads this yet. it is the other half of an "admin class nobody
/// registers" diagnostic, which is written and not shipped: a check that fires
/// in the editor and never in `by check` teaches people to distrust both, and
/// making it reportable there means reaching the project's files from below
/// `ty_project`. see `scratch.django.md` for the whole of it.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn admin_registrations(db: &dyn Db, project: Project) -> AdminRegistrations {
    let mut registered = Vec::new();
    let mut complete = true;

    for file in &project.files(db) {
        // a stub declares types, never a registration django runs
        if is_stub(db, file) {
            continue;
        }

        let found = admin_registrations_in_file(db, file);
        registered.extend(found.registered.iter().copied());
        complete &= found.complete;
    }

    AdminRegistrations {
        registered: registered.into_boxed_slice(),
        complete,
    }
}

/// the admin classes one module registers
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn admin_registrations_in_file(db: &dyn Db, file: File) -> AdminRegistrations {
    // both forms — `admin.site.register(…)` and `@admin.register(…)` — spell the
    // method itself, and a module that imports it under another name still
    // writes the real one beside the alias
    if !mentions(db, file, &[ADMIN_REGISTER_METHOD]) {
        return AdminRegistrations::default();
    }

    let parsed = parsed_module(db, file).load(db);
    let mut visitor = AdminRegistrationVisitor {
        db,
        file,
        decorators: FxHashSet::default(),
        registered: Vec::new(),
        complete: true,
    };
    visitor.visit_body(parsed.suite());

    AdminRegistrations {
        registered: visitor.registered.into_boxed_slice(),
        complete: visitor.complete,
    }
}

/// what an argument of a `register(…)` call names
enum Class {
    /// a django model
    Model,
    /// a django admin class
    Admin(ClassRef),
    /// a class django gives no role to, or a value that is plainly no class
    Other,
    /// nothing this could work out
    Unreadable,
}

struct AdminRegistrationVisitor<'db> {
    db: &'db dyn Db,
    file: File,
    /// the `register(…)` calls already read as decorators
    ///
    /// a decorator is an expression like any other, so without this the walk
    /// would read `@admin.register(Book)` a second time as a call registering
    /// django's own default admin for `Book`.
    decorators: FxHashSet<TextRange>,
    registered: Vec<ClassRef>,
    complete: bool,
}

impl AdminRegistrationVisitor<'_> {
    /// the registration a `@admin.register(…)` on `class` makes
    ///
    /// this form names the models in the decorator and the admin class by
    /// decorating it, so which class is registered is beyond doubt however the
    /// models are written.
    fn decorated(&mut self, class: &ast::StmtClassDef) {
        for decorator in &class.decorator_list {
            let Expr::Call(call) = &decorator.expression else {
                continue;
            };
            if callee_name(&call.func).as_deref() != Some(ADMIN_REGISTER_METHOD) {
                continue;
            }
            // a registration is never written as a decorator in the other form,
            // so whatever this is, the call walk has nothing to add to it
            self.decorators.insert(call.range());

            if django_class_kind(self.db, self.file, class, MAX_BASE_DEPTH)
                != Some(DjangoClassKind::Admin)
            {
                continue;
            }

            if call.arguments.args.iter().all(|model| self.is_model(model)) {
                self.registered.push(ClassRef {
                    file: self.file,
                    range: class.name.range(),
                });
            } else {
                self.complete = false;
            }
        }
    }

    /// the registration a `site.register(…)` call makes
    ///
    /// what identifies one is the shape of its arguments rather than the site it
    /// is made on: a project registers as readily against an `AdminSite` of its
    /// own as against django's, and nothing about the receiver says which.
    ///
    /// so a call is one of these unless it can be told apart from one. an
    /// argument that resolves to a class django gives no role to is what tells
    /// it apart — a rest framework router's viewset, a `singledispatch`'s type —
    /// as is a route prefix written where django takes a model. short of that
    /// the call counts, and a call that counts and cannot be read costs the
    /// whole check.
    fn register_call(&mut self, call: &ast::ExprCall) {
        if self.decorators.contains(&call.range())
            || callee_name(&call.func).as_deref() != Some(ADMIN_REGISTER_METHOD)
        {
            return;
        }

        // django's signature is `register(model_or_iterable, admin_class=None)`:
        // without the second argument the model is given django's own default
        // admin, and no class of the project's is named at all
        let (Some(model), Some(admin)) = (
            call.arguments.args.first(),
            call.arguments.find_argument_value(ADMIN_CLASS_KEYWORD, 1),
        ) else {
            return;
        };

        let (model_class, admin_class) = (self.resolved(model), self.resolved(admin));

        let is_someone_elses = matches!(model_class, Class::Other)
            || matches!(admin_class, Class::Other | Class::Model);
        if is_someone_elses {
            return;
        }

        // an argument that cannot be read leaves an admin class registered that
        // nothing here can match, so the answer is that there is no answer
        let names_a_model = matches!(model_class, Class::Model) || self.is_model(model);
        let Class::Admin(registered) = admin_class else {
            self.complete = false;
            return;
        };
        if !names_a_model {
            self.complete = false;
            return;
        }

        self.registered.push(registered);
    }

    /// whether `expr` names a django model, or a list of them
    fn is_model(&self, expr: &Expr) -> bool {
        match expr {
            Expr::List(list) => list.elts.iter().all(|element| self.is_model(element)),
            Expr::Tuple(tuple) => tuple.elts.iter().all(|element| self.is_model(element)),
            _ => matches!(self.resolved(expr), Class::Model),
        }
    }

    /// what the class `expr` names is, where it names one at all
    fn resolved(&self, expr: &Expr) -> Class {
        // django takes a model where a router takes its route prefix, and a
        // string is not something this failed to read
        if matches!(expr, Expr::StringLiteral(_)) {
            return Class::Other;
        }

        resolved_class(self.db, self.file, expr, |defining, class| {
            let reference = ClassRef {
                file: defining,
                range: class.name.range(),
            };

            match django_class_kind(self.db, defining, class, MAX_BASE_DEPTH) {
                Some(DjangoClassKind::Model) => Class::Model,
                Some(DjangoClassKind::Admin) => Class::Admin(reference),
                None => Class::Other,
            }
        })
        .unwrap_or(Class::Unreadable)
    }
}

impl<'ast> Visitor<'ast> for AdminRegistrationVisitor<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::ClassDef(class) = stmt {
            self.decorated(class);
        }

        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            self.register_call(call);
        }

        walk_expr(self, expr);
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

    use crate::django_template::tests::{DJANGO_BUILTINS, TemplateTest, with_forward_slashes};

    use super::{
        RegistrationKind, context_for_template, context_processor_variables,
        django_is_authoritative, registrations, static_files, tag_libraries, template_files,
        url_names,
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
            .map(|file| with_forward_slashes(format_args!("{} -> {}", file.name, file.path)))
            .collect()
    }

    /// every static file found, as `name -> path`
    fn statics(test: &TemplateTest) -> Vec<String> {
        static_files(&test.db, test.db.project())
            .iter()
            .map(|file| with_forward_slashes(format_args!("{} -> {}", file.name, file.path)))
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

    /// the names the project's context processors put in every context
    fn processors(test: &TemplateTest) -> Vec<String> {
        context_processor_variables(&test.db, test.db.project())
            .iter()
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
    fn a_context_held_in_a_variable_is_followed_to_what_it_holds() {
        let test = project(&[(
            "app/views.py",
            "
            def show(request):
                context = {'book': 1, 'shelf': 2}
                return render(request, 'app/page.html', context)
            ",
        )]);

        assert_eq!(context(&test, "app/page.html"), ["book", "shelf"]);
    }

    #[test]
    fn a_context_is_followed_through_the_writes_and_updates_that_build_it() {
        let test = project(&[(
            "app/views.py",
            "
            def show(request):
                context = {'book': 1}
                context['extra'] = 2
                context.update({'more': 3})
                return render(request, 'app/page.html', context)
            ",
        )]);

        assert_eq!(context(&test, "app/page.html"), ["book", "extra", "more"]);
    }

    #[test]
    fn a_context_spread_from_another_dict_carries_its_names_too() {
        let test = project(&[(
            "app/views.py",
            "
            def show(request):
                base = {'site': 1}
                context = {**base, 'book': 2}
                return render(request, 'app/page.html', context)
            ",
        )]);

        assert_eq!(context(&test, "app/page.html"), ["site", "book"]);
    }

    #[test]
    fn a_context_rebound_more_than_once_is_read_as_it_stands_at_the_render_call() {
        let test = project(&[(
            "app/views.py",
            "
            def show(request):
                context = {'first': 1}
                context = {'second': 2}
                return render(request, 'app/page.html', context)
            ",
        )]);

        assert_eq!(context(&test, "app/page.html"), ["second"]);
    }

    #[test]
    fn two_renders_of_one_context_each_see_what_had_run_by_then() {
        let test = project(&[(
            "app/views.py",
            "
            def show(request):
                context = {'book': 1}
                if request.GET:
                    return render(request, 'app/early.html', context)
                context['extra'] = 2
                return render(request, 'app/page.html', context)
            ",
        )]);

        assert_eq!(context(&test, "app/early.html"), ["book"]);
        assert_eq!(context(&test, "app/page.html"), ["book", "extra"]);
    }

    #[test]
    fn a_context_that_cannot_be_followed_contributes_nothing() {
        let test = project(&[(
            "app/views.py",
            "
            def show(request):
                context = build_context(request)
                context['extra'] = 1
                return render(request, 'app/page.html', context)
            ",
        )]);

        // the call is unreadable, but the write on top of it is not
        assert_eq!(context(&test, "app/page.html"), ["extra"]);
    }

    #[test]
    fn a_context_bound_outside_the_rendering_function_is_not_followed() {
        let test = project(&[(
            "app/views.py",
            "
            context = {'book': 1}

            def show(request):
                return render(request, 'app/page.html', context)
            ",
        )]);

        assert!(context(&test, "app/page.html").is_empty());
    }

    #[test]
    fn a_base_class_contributes_the_context_it_builds() {
        let test = project(&[(
            "app/views.py",
            "
            class BaseView:
                extra_context = {'year': 2026}

                def get_context_data(self, **kwargs):
                    context = super().get_context_data(**kwargs)
                    context['site'] = 1
                    return context

            class BookView(BaseView):
                template_name = 'app/detail.html'

                def get_context_data(self, **kwargs):
                    context = super().get_context_data(**kwargs)
                    context['book'] = 2
                    return context
            ",
        )]);

        assert_eq!(
            context(&test, "app/detail.html"),
            ["book", "year", "site"],
            "the view's own names come before the ones it inherits"
        );
    }

    #[test]
    fn a_base_class_in_another_module_contributes_too() {
        let test = project(&[
            (
                "app/base.py",
                "
                class BaseView:
                    def get_context_data(self, **kwargs):
                        context = super().get_context_data(**kwargs)
                        context['site'] = 1
                        return context
                ",
            ),
            (
                "app/views.py",
                "
                from app.base import BaseView

                class BookView(BaseView):
                    template_name = 'app/detail.html'
                ",
            ),
        ]);

        assert_eq!(context(&test, "app/detail.html"), ["site"]);
    }

    #[test]
    fn a_class_that_inherits_in_a_circle_is_read_once_and_terminates() {
        let test = project(&[(
            "app/views.py",
            "
            class First(Second):
                template_name = 'app/detail.html'
                extra_context = {'first': 1}

            class Second(First):
                extra_context = {'second': 2}
            ",
        )]);

        assert_eq!(context(&test, "app/detail.html"), ["first", "second"]);
    }

    #[test]
    fn a_get_context_data_returning_a_dict_outright_contributes_its_keys() {
        let test = project(&[(
            "app/views.py",
            "
            class BookView:
                template_name = 'app/detail.html'

                def get_context_data(self, **kwargs):
                    return {'book': 1, **super().get_context_data(**kwargs)}
            ",
        )]);

        assert_eq!(context(&test, "app/detail.html"), ["book"]);
    }

    #[test]
    fn a_context_processor_contributes_the_names_it_returns() {
        let test = configured(
            "
            TEMPLATES = [{'OPTIONS': {'context_processors': ['app.processors.branding']}}]
            ",
            &[(
                "app/processors.py",
                "
                def branding(request):
                    return {'site_name': 'a blog', 'year': 2026}
                ",
            )],
        );

        assert_eq!(processors(&test), ["site_name", "year"]);
    }

    #[test]
    fn a_context_processor_is_followed_through_the_variable_it_builds() {
        let test = configured(
            "
            TEMPLATES = [{'OPTIONS': {'context_processors': ['app.processors.branding']}}]
            ",
            &[(
                "app/processors.py",
                "
                def branding(request):
                    context = {'site_name': 'a blog'}
                    context['year'] = 2026
                    return context
                ",
            )],
        );

        assert_eq!(processors(&test), ["site_name", "year"]);
    }

    #[test]
    fn a_context_processor_whose_result_cannot_be_read_contributes_nothing() {
        let test = configured(
            "
            TEMPLATES = [{'OPTIONS': {'context_processors': [
                'app.processors.built',
                'app.processors.elsewhere',
                'app.processors.missing',
                'nowhere.at_all',
            ]}}]
            ",
            &[(
                "app/processors.py",
                "
                def built(request):
                    return dict(site_name='a blog')

                def elsewhere(request):
                    return other(request)
                ",
            )],
        );

        assert!(processors(&test).is_empty());
    }

    #[test]
    fn a_processor_writing_into_something_that_is_not_a_context_contributes_nothing() {
        let test = configured(
            "
            TEMPLATES = [{'OPTIONS': {'context_processors': ['app.processors.branding']}}]
            ",
            &[(
                "app/processors.py",
                "
                def branding(request):
                    request.session['cart'] = []
                    return {'site_name': 'a blog'}
                ",
            )],
        );

        assert_eq!(processors(&test), ["site_name"]);
    }

    #[test]
    fn the_last_context_processor_to_name_something_is_the_one_that_renders() {
        let test = configured(
            "
            TEMPLATES = [{'OPTIONS': {'context_processors': [
                'app.processors.first',
                'app.processors.second',
            ]}}]
            ",
            &[(
                "app/processors.py",
                "
                def first(request):
                    return {'site_name': 'one'}

                def second(request):
                    return {'site_name': 'two'}
                ",
            )],
        );

        let found = context_processor_variables(&test.db, test.db.project());
        assert_eq!(found.len(), 1);
        assert_eq!(
            &ruff_db::source::source_text(&test.db, found[0].file)[found[0].value.unwrap()],
            "'two'"
        );
    }

    #[test]
    fn a_project_that_names_no_context_processors_has_none() {
        let test = configured("TEMPLATES = [{'APP_DIRS': True}]", &[]);
        assert!(processors(&test).is_empty());
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

        assert_eq!(
            with_forward_slashes(action.file.path(&test.db)),
            "/app/views.py"
        );
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
            with_forward_slashes(intcomma.file.path(&test.db)),
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

    #[test]
    fn a_function_registered_under_two_names_answers_to_both() {
        // django's own `{% translate %}` and `{% trans %}` are one function
        // carrying two `@register.tag` decorators, and both spellings work
        let test = installed(
            "
            INSTALLED_APPS = []
            ",
            &[],
            &[
                ("django/__init__.py", ""),
                ("django/templatetags/__init__.py", ""),
                (
                    "django/templatetags/i18n.py",
                    "
                    from django.template import Library

                    register = Library()

                    @register.tag('translate')
                    @register.tag('trans')
                    def do_translate(parser, token): ...
                    ",
                ),
            ],
        );

        assert_eq!(
            registered(&test),
            ["i18n|translate (django)", "i18n|trans (django)"]
        );
    }

    #[test]
    fn djangos_implicit_builtins_are_discovered_as_libraries_nothing_has_to_load() {
        let test = installed(
            "
            INSTALLED_APPS = []
            ",
            &[],
            DJANGO_BUILTINS,
        );

        assert_eq!(
            libraries(&test),
            [
                "defaulttags (Django, always loaded)",
                "defaultfilters (Django, always loaded)",
                "loader_tags (Django, always loaded)",
            ],
            "`Engine.default_builtins` are libraries like any other, bar the loading"
        );
        assert_eq!(
            registered(&test),
            [
                "defaulttags|for (django)",
                "defaulttags|if (django)",
                "defaulttags|squish (django)",
                "defaultfilters|upper (django)",
                "defaultfilters|shorten (django)",
                "loader_tags|block (django)",
                "loader_tags|extends (django)",
                "loader_tags|include (django)",
            ]
        );
    }

    #[test]
    fn a_django_whose_builtins_were_read_is_authoritative() {
        let test = installed(
            "
            INSTALLED_APPS = []
            ",
            &[],
            DJANGO_BUILTINS,
        );

        assert!(django_is_authoritative(&test.db, test.db.project()));
    }

    #[test]
    fn a_django_that_cannot_be_read_is_never_authoritative() {
        // this mock has a `templatetags` package but no `django.template`, so
        // none of what django itself registers was read
        let test = installed(
            "
            INSTALLED_APPS = ['django.contrib.humanize']
            ",
            &[],
            DJANGO,
        );
        assert!(!django_is_authoritative(&test.db, test.db.project()));

        // and a project with no settings module reaches nothing installed at all
        let test = TemplateTest::with_site_packages(
            &[("app/templates/app/page.html", "<CURSOR>")],
            DJANGO_BUILTINS,
        );
        assert!(!django_is_authoritative(&test.db, test.db.project()));
    }
}
