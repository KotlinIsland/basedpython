//! what a route and the handler it names get wrong about each other
//!
//! a route pattern names its arguments, and the framework hands each of them to
//! the handler by keyword when the url is requested. nothing checks the two
//! against each other: django finds out at request time, and what it raises is a
//! `TypeError` naming neither the route nor the view usefully.
//!
//! the pairing is the whole subject here, and it is not django's alone —
//! `@api.get("/users/{user_id}")` serving an `async def get_user(user_id: int)`
//! is the same idea in another spelling. so what varies is factored out: how a
//! pattern writes its parameters is [`project::ParameterSyntax`]'s to say, what a
//! converter hands the handler is [`project::Converter`]'s, and everything below
//! works on a list of parameters and a callable. wiring a second framework up is
//! a scan that yields routes, not a second copy of this.
//!
//! as everywhere else in this module, the rule is that a diagnostic must never
//! fire on correct code. every way a handler could accept more than it appears to
//! — a `**kwargs`, a decorator the type checker cannot see through, an inherited
//! django handler, an extra argument the route itself passes — silences the
//! check rather than being reported against.

use compact_str::{CompactString, ToCompactString};
use ruff_db::diagnostic::{Annotation, Diagnostic, DiagnosticId, Span};
use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_python_ast::{self as ast, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_project::Db;
use ty_python_semantic::django_template::{INVALID_ROUTE_HANDLER, INVALID_ROUTE_PARAMETER_TYPE};
use ty_python_semantic::lint::{LintId, LintMetadata};
use ty_python_semantic::types::ide_support::{
    CallableParameter, CallableParameterKind, callable_parameters,
};
use ty_python_semantic::{HasType, SemanticModel};

use super::project::{self, Parameter, RouteView, TargetKind, UrlName};

/// the methods a django class-based view serves a request through
///
/// `dispatch` and `setup` are handed the route's arguments before the method for
/// the request's verb is, so all three see the same call. everything else a view
/// class declares — `get_context_data`, `get_queryset` — django calls itself,
/// with arguments of its own that have nothing to do with the route.
const HANDLER_METHODS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "head", "options", "trace", "setup", "dispatch",
];

/// everything wrong with the routes `file` declares
///
/// nothing is reported unless the url tree was walked in full: a route's
/// arguments include the ones contributed by every pattern it is mounted behind,
/// and a walk that stopped short has read only some of them.
pub(crate) fn diagnostics(db: &dyn Db, file: File) -> Vec<Diagnostic> {
    if !project::routes_are_authoritative(db, db.project()) {
        return Vec::new();
    }

    let mut found = Vec::new();

    for route in project::url_names(db, db.project())
        .iter()
        .filter(|route| route.file == file)
    {
        check_route(db, file, route, &mut found);
    }

    found.sort_by_key(|diagnostic| {
        diagnostic
            .primary_span()
            .and_then(|span| span.range())
            .map(TextRange::start)
            .unwrap_or_default()
    });
    found
}

/// check one route against every handler it reaches
fn check_route(db: &dyn Db, file: File, route: &UrlName, found: &mut Vec<Diagnostic>) {
    // a view nothing could resolve to a definition, and a pattern this could not
    // read in full, are both routes there is nothing to say about
    let (Some(view), Some(parameters)) = (route.view.as_ref(), route.parameters()) else {
        return;
    };

    for handler in handlers(db, view) {
        for complaint in handler.complaints(db, &parameters, route.extra_arguments) {
            report(db, file, view, &handler, &complaint, found);
        }
    }
}

/// record one complaint, unless the rule is off or an ignore comment covers it
///
/// the diagnostic is anchored where the route names the view rather than at the
/// view itself: one view may serve several routes, and it is the pairing that is
/// wrong rather than either half. where the handler is somewhere else — a method
/// of a view class, a function in another module — a secondary annotation points
/// at it.
fn report(
    db: &dyn Db,
    file: File,
    view: &RouteView,
    handler: &Handler,
    complaint: &Complaint,
    found: &mut Vec<Diagnostic>,
) {
    let lint = complaint.lint();
    let Some(severity) = db.rule_selection(file).severity(LintId::of(lint)) else {
        return;
    };

    let mut diagnostic = Diagnostic::new(
        DiagnosticId::Lint(lint.name()),
        severity,
        format_args!("`{}` {}", handler.name, complaint.message),
    );
    diagnostic.annotate(Annotation::primary(Span::from(file).with_range(view.range)));
    diagnostic.annotate(
        Annotation::secondary(Span::from(handler.file).with_range(handler.range))
            .message(format_args!("`{}` is declared here", handler.name)),
    );
    diagnostic.help(complaint.help.clone());

    found.push(diagnostic);
}

/// what serves a request the route hands over
///
/// a function view is the one thing django calls. a class reached through
/// `as_view()` is not called at all — what django calls is the method for the
/// request's verb, so each of those is a handler, wherever in the class's
/// hierarchy it is declared. what a project inherits from django needs no
/// exception: every handler django ships takes `**kwargs`, and a `**kwargs`
/// silences the check on its own.
fn handlers<'db>(db: &'db dyn Db, view: &RouteView) -> Vec<Handler<'db>> {
    let parsed = parsed_module(db, view.target.file).load(db);

    let declaration = parsed
        .suite()
        .iter()
        .find_map(|statement| declaration_at(statement, view.target.range));

    match (view.target.kind, declaration) {
        (TargetKind::Function, Some(Declaration::Function(function))) => {
            Handler::of(db, view.target.file, function, None)
                .into_iter()
                .collect()
        }
        (TargetKind::Class, Some(Declaration::Class(class))) if view.class_based => HANDLER_METHODS
            .iter()
            .filter_map(|method| {
                match declared_handler(db, view.target.file, class, method, MAX_BASE_DEPTH)? {
                    Declared::Handler(handler) => Some(handler),
                    Declared::Anything => None,
                }
            })
            .collect(),
        // a class a route hands the request to *without* `as_view()` is called
        // rather than dispatched to, and what django would do with the result is
        // not something this can answer
        _ => Vec::new(),
    }
}

/// how many base classes deep a handler is looked for
///
/// a project's own hierarchy is a couple deep and django's own another couple,
/// so this is far past anything real — it is here so that a class that somehow
/// inherits from itself stops the walk rather than running for ever.
const MAX_BASE_DEPTH: usize = 16;

/// what a class says about the method a route's request reaches
enum Declared<'db> {
    /// something in the hierarchy declares it, and this is what it takes
    Handler(Handler<'db>),
    /// something declares it and it accepts whatever it is given
    Anything,
}

/// the declaration of `method` django would call on `class`, bases included
///
/// only the *first* declaration counts: a subclass overriding `get` is the one
/// django calls and a base's is never reached, so the bases are tried in the
/// order python resolves a member through them. a base that cannot be followed
/// stops the search rather than being walked past — it may be the very one that
/// declares the method, and what a search cannot see it must not report around.
fn declared_handler<'db>(
    db: &'db dyn Db,
    file: File,
    class: &ast::StmtClassDef,
    method: &str,
    depth: usize,
) -> Option<Declared<'db>> {
    if let Some(function) = class.body.iter().find_map(|statement| match statement {
        Stmt::FunctionDef(function) if function.name.as_str() == method => Some(function),
        _ => None,
    }) {
        return Some(
            match Handler::of(db, file, function, Some(class.name.id.as_str())) {
                Some(handler) => Declared::Handler(handler),
                None => Declared::Anything,
            },
        );
    }

    if depth == 0 {
        return Some(Declared::Anything);
    }

    for base in class.bases() {
        let followed = project::resolved_class(db, file, base, |defining, base_class| {
            declared_handler(db, defining, base_class, method, depth - 1)
        });

        match followed {
            Some(Some(found)) => return Some(found),
            // the base was read and declares nothing of the name
            Some(None) => {}
            None => return Some(Declared::Anything),
        }
    }

    None
}

/// a declaration a target's name range points at
enum Declaration<'ast> {
    Function(&'ast ast::StmtFunctionDef),
    Class(&'ast ast::StmtClassDef),
}

/// the declaration whose own name occupies `range`, wherever it is nested
fn declaration_at(statement: &Stmt, range: TextRange) -> Option<Declaration<'_>> {
    match statement {
        Stmt::FunctionDef(function) if function.name.range() == range => {
            Some(Declaration::Function(function))
        }
        Stmt::ClassDef(class) if class.name.range() == range => Some(Declaration::Class(class)),
        Stmt::FunctionDef(function) if function.range().contains_range(range) => function
            .body
            .iter()
            .find_map(|nested| declaration_at(nested, range)),
        Stmt::ClassDef(class) if class.range().contains_range(range) => class
            .body
            .iter()
            .find_map(|nested| declaration_at(nested, range)),
        _ => None,
    }
}

/// one callable a route's request reaches, and the parameters the route fills
struct Handler<'db> {
    /// what a diagnostic calls it, qualified by the view class where there is one
    name: CompactString,
    file: File,
    /// the declared name, for a secondary annotation
    range: TextRange,
    /// every parameter after the request, which is the one django passes
    /// positionally and no route ever names
    parameters: Vec<CallableParameter<'db>>,
}

impl<'db> Handler<'db> {
    /// the handler `function` is, or `None` where it accepts anything
    ///
    /// this is where every reason to say nothing is decided. the signature comes
    /// from the type checker rather than from the parameter list as written, so
    /// it is the decorated callable django will really call: a decorator that
    /// hands back what it was given keeps the view's own parameters, and one the
    /// type checker cannot see through leaves a signature this refuses to read.
    fn of(
        db: &'db dyn Db,
        file: File,
        function: &ast::StmtFunctionDef,
        class: Option<&str>,
    ) -> Option<Self> {
        let declared = function.inferred_type(&SemanticModel::new(db, file))?;
        let mut parameters = callable_parameters(db, declared)?;

        // a method is called through the instance, which fills its receiver
        if class.is_some() && !parameters.is_empty() {
            parameters.remove(0);
        }

        // the request goes first and positionally, so a handler with nothing to
        // put it in is one django cannot call at all — a different complaint,
        // and not one a route's arguments can be held against
        let request = parameters.first()?;
        if !matches!(
            request.kind,
            CallableParameterKind::PositionalOnly | CallableParameterKind::PositionalOrKeyword
        ) {
            return None;
        }
        parameters.remove(0);

        // `*args` or `**kwargs` takes whatever it is given, named or not
        if parameters.iter().any(|parameter| {
            matches!(
                parameter.kind,
                CallableParameterKind::Variadic | CallableParameterKind::KeywordVariadic
            )
        }) {
            return None;
        }

        Some(Self {
            name: match class {
                Some(class) => format!("{class}.{}", function.name).to_compact_string(),
                None => function.name.id.to_compact_string(),
            },
            file,
            range: function.name.range(),
            parameters,
        })
    }

    /// everything wrong between this handler and a route giving it `parameters`
    fn complaints(
        &self,
        db: &'db dyn Db,
        parameters: &[Parameter],
        extra_arguments: bool,
    ) -> Vec<Complaint> {
        let mut complaints = Vec::new();
        let mut turned_one_down = false;

        for parameter in parameters {
            let Some(accepted) = self.by_keyword(&parameter.name) else {
                turned_one_down = true;
                complaints.push(Complaint {
                    kind: ComplaintKind::Handler,
                    message: format!("takes no argument named `{}`", parameter.name),
                    help: "the route names it, so django passes it".to_string(),
                });
                continue;
            };

            // only a declared annotation is answered for: a bare parameter
            // declares nothing, and neither does an argument matched by a regular
            // expression rather than put through a converter
            let (Some(converter), Some(declared)) = (parameter.converter, accepted.declared_type)
            else {
                continue;
            };
            let Some(value) = parameter.value_type(db) else {
                continue;
            };
            if value.is_assignable_to(db, declared) {
                continue;
            }

            complaints.push(Complaint {
                kind: ComplaintKind::ParameterType,
                message: format!("takes `{}` as `{}`", parameter.name, declared.display(db)),
                help: format!(
                    "the route's `{}` converter gives a `{}`",
                    converter.name(),
                    value.display(db)
                ),
            });
        }

        // a route passing arguments of its own may be filling anything the
        // pattern says nothing about. and a name the handler already turned down
        // is one misspelling reported once: saying it takes no `pk` *and* needs
        // an `id` is the same mistake from both ends
        if extra_arguments || turned_one_down {
            return complaints;
        }

        complaints.extend(
            self.parameters
                .iter()
                .filter(|parameter| !parameter.has_default)
                .filter_map(|parameter| parameter.name.as_ref())
                .filter(|name| !parameters.iter().any(|given| given.name == name.as_str()))
                .map(|name| Complaint {
                    kind: ComplaintKind::Handler,
                    message: format!("needs an argument named `{name}`"),
                    help: "the route names no such argument".to_string(),
                }),
        );

        complaints
    }

    /// the parameter of this name a caller could fill by keyword
    ///
    /// a positional-only parameter is deliberately not one: django passes what a
    /// pattern captured by keyword, so a name it cannot be passed under is a name
    /// the handler does not take.
    fn by_keyword(&self, name: &str) -> Option<&CallableParameter<'db>> {
        self.parameters.iter().find(|parameter| {
            matches!(
                parameter.kind,
                CallableParameterKind::PositionalOrKeyword | CallableParameterKind::KeywordOnly
            ) && parameter
                .name
                .as_ref()
                .is_some_and(|declared| declared.as_str() == name)
        })
    }
}

/// one thing wrong between a route and its handler
struct Complaint {
    kind: ComplaintKind,
    message: String,
    help: String,
}

/// which of the two rules a complaint belongs to
enum ComplaintKind {
    /// django cannot call the handler at all
    Handler,
    /// django calls it with a value its annotation says it never gets
    ParameterType,
}

impl Complaint {
    fn lint(&self) -> &'static LintMetadata {
        match self.kind {
            ComplaintKind::Handler => &INVALID_ROUTE_HANDLER,
            ComplaintKind::ParameterType => &INVALID_ROUTE_PARAMETER_TYPE,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::django_template::tests::TemplateTest;

    /// a django project whose url tree can be walked in full
    ///
    /// the walk is what every check here is gated on, so the settings name a
    /// `ROOT_URLCONF` and every include below it resolves. `urls` and `views` are
    /// the two halves under test.
    fn project(urls: &str, views: &str) -> TemplateTest {
        TemplateTest::with_site_packages(
            &[
                (
                    "manage.py",
                    "
                    import os

                    os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'project.settings')
                    ",
                ),
                ("project/__init__.py", ""),
                (
                    "project/settings.py",
                    "
                    INSTALLED_APPS = ['blog']

                    TEMPLATES = [{'DIRS': [], 'APP_DIRS': True, 'OPTIONS': {}}]

                    ROOT_URLCONF = 'project.urls'
                    ",
                ),
                (
                    "project/urls.py",
                    "
                    from django.urls import include, path

                    urlpatterns = [path('blog/', include('blog.urls'))]
                    ",
                ),
                ("blog/__init__.py", ""),
                ("blog/wrappers.py", WRAPPERS),
                ("blog/views.py", views),
                ("blog/urls.py", urls),
            ],
            &[
                ("django/__init__.py", ""),
                ("django/urls/__init__.py", DJANGO_URLS),
                ("django/views/__init__.py", ""),
                ("django/views/generic/__init__.py", DJANGO_GENERIC),
            ],
        )
    }

    /// the two kinds of decorator a view is written behind
    ///
    /// one hands back what it was given and one hands back something the type
    /// checker cannot see into, which is the whole of what decides whether a
    /// decorated view is answered for.
    const WRAPPERS: &str = "
        from typing import Any


        def passthrough[F](view: F) -> F: ...


        def wrap(view) -> Any: ...
        ";

    /// just enough of `django.urls` for a route to be written against it
    const DJANGO_URLS: &str = "
        def path(route, view, kwargs=None, name=None): ...

        def re_path(route, view, kwargs=None, name=None): ...

        def include(arg, namespace=None): ...
        ";

    /// django's own class-based views, whose handlers all take `**kwargs`
    const DJANGO_GENERIC: &str = "
        class View:
            def setup(self, request, *args, **kwargs): ...

            def dispatch(self, request, *args, **kwargs): ...


        class TemplateView(View):
            def get(self, request, *args, **kwargs): ...
        ";

    /// the urls every test that isn't about the url configuration itself uses
    fn urls_for(pattern: &str, view: &str) -> String {
        format!(
            "
            from django.urls import path, re_path

            from blog import views

            app_name = 'blog'

            urlpatterns = [path('{pattern}', views.{view}, name='detail')]
            "
        )
    }

    /// the diagnostics of a route written out in full, with its views
    fn check_urls(urls: &str, views: &str) -> Vec<String> {
        project(urls, views).python_diagnostics("blog/urls.py")
    }

    /// the same for the one-route case, which is most of them
    fn check(pattern: &str, view: &str, views: &str) -> Vec<String> {
        check_urls(&urls_for(pattern, view), views)
    }

    // ---- the pairing is right -----------------------------------------------

    #[test]
    fn a_view_taking_the_route_s_argument_by_name_and_type_is_not_reported() {
        assert_eq!(
            check(
                "<int:pk>/",
                "detail",
                "
                def detail(request, pk: int): ...
                ",
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_view_taking_every_argument_of_a_route_with_several_is_not_reported() {
        assert_eq!(
            check(
                "<slug:slug>/<int:page>/",
                "paged",
                "
                def paged(request, slug: str, page: int): ...
                ",
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_view_of_a_route_taking_nothing_is_not_reported() {
        assert_eq!(
            check(
                "",
                "index",
                "
                def index(request): ...
                ",
            ),
            Vec::<String>::new()
        );
    }

    // ---- the pairing is wrong -----------------------------------------------

    #[test]
    fn a_view_naming_the_argument_differently_is_reported() {
        assert_eq!(
            check(
                "<int:pk>/",
                "detail",
                "
                def detail(request, id: int): ...
                ",
            ),
            [
                "invalid-route-handler Error: `detail` takes no argument named `pk` \
                 [views.detail]"
            ]
        );
    }

    #[test]
    fn a_view_not_taking_the_argument_at_all_is_reported() {
        assert_eq!(
            check(
                "<int:pk>/",
                "detail",
                "
                def detail(request): ...
                ",
            ),
            [
                "invalid-route-handler Error: `detail` takes no argument named `pk` \
                 [views.detail]"
            ]
        );
    }

    #[test]
    fn a_view_whose_extra_parameter_has_a_default_is_not_reported() {
        assert_eq!(
            check(
                "",
                "index",
                "
                def index(request, category=None): ...
                ",
            ),
            Vec::<String>::new(),
            "django leaves it out and the default stands"
        );
    }

    #[test]
    fn a_keyword_only_parameter_is_one_a_route_can_fill() {
        assert_eq!(
            check(
                "<int:pk>/",
                "detail",
                "
                def detail(request, *, pk: int): ...
                ",
            ),
            Vec::<String>::new(),
            "django passes what the pattern captured by keyword"
        );
    }

    #[test]
    fn a_positional_only_parameter_is_one_it_cannot() {
        assert_eq!(
            check(
                "<int:pk>/",
                "detail",
                "
                def detail(request, pk, /): ...
                ",
            ),
            ["invalid-route-handler Error: `detail` takes no argument named `pk` [views.detail]"],
            "django passes it by keyword, and a positional-only name cannot be written"
        );
    }

    #[test]
    fn a_view_needing_an_argument_the_route_does_not_name_is_reported() {
        assert_eq!(
            check(
                "",
                "index",
                "
                def index(request, category): ...
                ",
            ),
            [
                "invalid-route-handler Error: `index` needs an argument named `category` \
                 [views.index]"
            ]
        );
    }

    #[test]
    fn a_view_declaring_a_type_the_converter_does_not_produce_is_reported() {
        assert_eq!(
            check(
                "<int:pk>/",
                "detail",
                "
                def detail(request, pk: str): ...
                ",
            ),
            [
                "invalid-route-parameter-type Warning: `detail` takes `pk` as `str` \
                 [views.detail]"
            ]
        );
    }

    #[test]
    fn every_converter_answers_for_the_type_it_produces() {
        let rejected = [
            ("<int:a>/", "a: str", "`a` as `str`"),
            ("<str:a>/", "a: int", "`a` as `int`"),
            ("<slug:a>/", "a: int", "`a` as `int`"),
            ("<path:a>/", "a: int", "`a` as `int`"),
            ("<uuid:a>/", "a: str", "`a` as `str`"),
        ];

        for (pattern, parameter, complaint) in rejected {
            assert_eq!(
                check(
                    pattern,
                    "detail",
                    &format!(
                        "
                        def detail(request, {parameter}): ...
                        "
                    ),
                ),
                [format!(
                    "invalid-route-parameter-type Warning: `detail` takes {complaint} \
                     [views.detail]"
                )],
                "{pattern}"
            );
        }
    }

    #[test]
    fn a_parameter_the_converter_s_value_fits_is_not_reported() {
        let accepted = [
            ("<int:a>/", "a: int"),
            ("<str:a>/", "a: str"),
            ("<slug:a>/", "a: str"),
            ("<path:a>/", "a: str"),
            ("<int:a>/", "a: object"),
            ("<int:a>/", "a: int | None"),
            ("<int:a>/", "a: float"),
        ];

        for (pattern, parameter) in accepted {
            assert_eq!(
                check(
                    pattern,
                    "detail",
                    &format!(
                        "
                        def detail(request, {parameter}): ...
                        "
                    ),
                ),
                Vec::<String>::new(),
                "{pattern}"
            );
        }
    }

    #[test]
    fn a_uuid_argument_declared_as_a_uuid_is_not_reported() {
        assert_eq!(
            check(
                "<uuid:key>/",
                "detail",
                "
                from uuid import UUID


                def detail(request, key: UUID): ...
                ",
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_view_serving_two_routes_is_answered_for_each_of_them() {
        let test = project(
            "
            from django.urls import path

            from blog import views

            app_name = 'blog'

            urlpatterns = [
                path('<int:pk>/', views.listing, name='detail'),
                path('', views.listing, name='index'),
            ]
            ",
            "
            def listing(request, pk: int): ...
            ",
        );

        assert_eq!(
            test.python_diagnostics("blog/urls.py"),
            ["invalid-route-handler Error: `listing` needs an argument named `pk` [views.listing]"],
            "the route naming nothing is served fine; the one naming `pk` is not"
        );
    }

    // ---- a route's arguments include what it is mounted behind ---------------

    #[test]
    fn an_argument_the_including_pattern_names_is_one_the_view_has_to_take() {
        let test = project(
            "
            from django.urls import path

            from blog import views

            app_name = 'blog'

            urlpatterns = [path('reply/', views.reply, name='reply')]
            ",
            "
            def reply(request): ...
            ",
        );
        let mut test = test;
        test.rewrite(
            "project/urls.py",
            "
            from django.urls import include, path

            urlpatterns = [path('blog/<int:pk>/', include('blog.urls'))]
            ",
        );

        assert_eq!(
            test.python_diagnostics("blog/urls.py"),
            ["invalid-route-handler Error: `reply` takes no argument named `pk` [views.reply]"]
        );
    }

    // ---- what must not fire -------------------------------------------------

    #[test]
    fn a_view_taking_keyword_variadics_is_not_reported() {
        assert_eq!(
            check(
                "<int:pk>/",
                "detail",
                "
                def detail(request, **kwargs): ...
                ",
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_view_taking_variadics_is_not_reported() {
        assert_eq!(
            check(
                "<int:pk>/",
                "detail",
                "
                def detail(request, *args): ...
                ",
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_unannotated_parameter_is_checked_by_name_and_never_by_type() {
        assert_eq!(
            check(
                "<int:pk>/",
                "detail",
                "
                def detail(request, pk): ...
                ",
            ),
            Vec::<String>::new(),
            "nothing declares a type, so there is no type to be wrong"
        );
        assert_eq!(
            check(
                "<int:pk>/",
                "detail",
                "
                def detail(request, id): ...
                ",
            ),
            ["invalid-route-handler Error: `detail` takes no argument named `pk` [views.detail]"],
            "the name is still the name"
        );
    }

    #[test]
    fn a_view_reached_through_a_decorator_nothing_can_see_through_is_not_reported() {
        assert_eq!(
            check(
                "<int:pk>/",
                "detail",
                "
                from blog.wrappers import wrap


                @wrap
                def detail(request): ...
                ",
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_view_behind_a_decorator_that_hands_the_view_back_is_still_checked() {
        let test = project(
            &urls_for("<int:pk>/", "detail"),
            "
            from blog.wrappers import passthrough


            @passthrough
            def detail(request): ...
            ",
        );

        assert_eq!(
            test.python_diagnostics("blog/urls.py"),
            ["invalid-route-handler Error: `detail` takes no argument named `pk` [views.detail]"],
            "the decorator is typed as giving back what it was handed"
        );
    }

    #[test]
    fn a_route_passing_arguments_of_its_own_is_not_reported_for_needing_them() {
        assert_eq!(
            check_urls(
                "
                from django.urls import path

                from blog import views

                app_name = 'blog'

                urlpatterns = [path('', views.index, {'category': 'books'}, name='index')]
                ",
                "
                def index(request, category): ...
                ",
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_view_nothing_can_resolve_is_not_reported() {
        assert_eq!(
            check_urls(
                "
                from django.urls import path

                from blog.nowhere import missing

                app_name = 'blog'

                urlpatterns = [path('<int:pk>/', missing, name='detail')]
                ",
                "
                def detail(request): ...
                ",
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_regex_group_is_checked_by_name_and_never_by_type() {
        assert_eq!(
            check_urls(
                "
                from django.urls import re_path

                from blog import views

                app_name = 'blog'

                urlpatterns = [
                    re_path(r'^archive/(?P<year>[0-9]{4})/$', views.archive, name='archive'),
                ]
                ",
                "
                def archive(request, year: int): ...
                ",
            ),
            Vec::<String>::new(),
            "a group goes through no converter, so nothing says what `year` is"
        );
        assert_eq!(
            check_urls(
                "
                from django.urls import re_path

                from blog import views

                app_name = 'blog'

                urlpatterns = [
                    re_path(r'^archive/(?P<year>[0-9]{4})/$', views.archive, name='archive'),
                ]
                ",
                "
                def archive(request, yr): ...
                ",
            ),
            [
                "invalid-route-handler Error: `archive` takes no argument named `year` \
                 [views.archive]"
            ]
        );
    }

    #[test]
    fn a_pattern_with_an_unnamed_group_is_not_reported() {
        assert_eq!(
            check_urls(
                "
                from django.urls import re_path

                from blog import views

                app_name = 'blog'

                urlpatterns = [
                    re_path(r'^archive/([0-9]{4})/$', views.archive, name='archive'),
                ]
                ",
                "
                def archive(request): ...
                ",
            ),
            Vec::<String>::new(),
            "django passes it positionally under no name at all"
        );
    }

    #[test]
    fn a_project_whose_url_tree_cannot_be_walked_reports_nothing() {
        let test = project(
            &urls_for("<int:pk>/", "detail"),
            "
            def detail(request): ...
            ",
        );
        let mut test = test;
        test.rewrite(
            "project/urls.py",
            "
            from django.urls import include, path

            urlpatterns = [path('blog/', include('nowhere.urls'))]
            ",
        );

        assert_eq!(
            test.python_diagnostics("blog/urls.py"),
            Vec::<String>::new()
        );
    }

    // ---- class-based views --------------------------------------------------

    #[test]
    fn a_class_based_view_whose_own_handler_takes_the_argument_is_not_reported() {
        assert_eq!(
            check(
                "<int:pk>/",
                "DetailView.as_view()",
                "
                from django.views.generic import TemplateView


                class DetailView(TemplateView):
                    def get(self, request, pk: int): ...
                ",
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_class_based_view_whose_own_handler_does_not_is_reported() {
        assert_eq!(
            check(
                "<int:pk>/",
                "DetailView.as_view()",
                "
                from django.views.generic import TemplateView


                class DetailView(TemplateView):
                    def get(self, request, slug): ...
                ",
            ),
            [
                "invalid-route-handler Error: `DetailView.get` takes no argument named `pk` \
                 [views.DetailView]"
            ]
        );
    }

    #[test]
    fn a_class_based_view_that_declares_no_handler_of_its_own_is_not_reported() {
        assert_eq!(
            check(
                "<int:pk>/",
                "DetailView.as_view()",
                "
                from django.views.generic import TemplateView


                class DetailView(TemplateView):
                    template_name = 'blog/detail.html'
                ",
            ),
            Vec::<String>::new(),
            "what it inherits from django takes `**kwargs`"
        );
    }

    #[test]
    fn a_class_based_view_whose_handler_takes_variadics_is_not_reported() {
        assert_eq!(
            check(
                "<int:pk>/",
                "DetailView.as_view()",
                "
                from django.views.generic import TemplateView


                class DetailView(TemplateView):
                    def get(self, request, *args, **kwargs): ...
                ",
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_class_based_view_s_other_methods_are_left_alone() {
        assert_eq!(
            check(
                "<int:pk>/",
                "DetailView.as_view()",
                "
                from django.views.generic import TemplateView


                class DetailView(TemplateView):
                    def get(self, request, pk: int): ...

                    def get_context_data(self, **kwargs): ...

                    def get_object(self): ...
                ",
            ),
            Vec::<String>::new(),
            "django calls those itself, with arguments of its own"
        );
    }

    #[test]
    fn a_handler_a_view_class_inherits_from_a_base_of_its_own_is_reported() {
        assert_eq!(
            check(
                "<int:pk>/",
                "DetailView.as_view()",
                "
                from django.views.generic import TemplateView


                class SiteMixin(TemplateView):
                    def get(self, request, slug): ...


                class DetailView(SiteMixin):
                    template_name = 'blog/detail.html'
                ",
            ),
            [
                "invalid-route-handler Error: `SiteMixin.get` takes no argument named `pk` \
                 [views.DetailView]"
            ],
            "django calls the mixin's `get`, so it is the one that has to take it"
        );
    }

    #[test]
    fn a_handler_the_class_itself_overrides_is_the_one_answered_for() {
        assert_eq!(
            check(
                "<int:pk>/",
                "DetailView.as_view()",
                "
                from django.views.generic import TemplateView


                class SiteMixin(TemplateView):
                    def get(self, request, slug): ...


                class DetailView(SiteMixin):
                    def get(self, request, pk: int): ...
                ",
            ),
            Vec::<String>::new(),
            "the base's `get` is never reached"
        );
    }

    #[test]
    fn a_view_class_with_a_base_nothing_can_follow_is_not_reported() {
        assert_eq!(
            check(
                "<int:pk>/",
                "DetailView.as_view()",
                "
                from blog.nowhere import Elsewhere


                class DetailView(Elsewhere):
                    template_name = 'blog/detail.html'
                ",
            ),
            Vec::<String>::new(),
            "a base that cannot be read may be the one declaring the handler"
        );
    }

    #[test]
    fn a_class_a_route_hands_the_request_to_directly_is_not_reported() {
        assert_eq!(
            check(
                "<int:pk>/",
                "DetailView",
                "
                from django.views.generic import TemplateView


                class DetailView(TemplateView):
                    def get(self, request, slug): ...
                ",
            ),
            Vec::<String>::new(),
            "without `as_view()` django calls the class rather than dispatching to it"
        );
    }

    // ---- silencing ----------------------------------------------------------

    /// a route the view cannot serve, silenced by an ignore comment
    const SILENCED: (&str, &str) = (
        "
        from django.urls import path

        from blog import views

        app_name = 'blog'

        urlpatterns = [
            path('<int:pk>/', views.detail, name='detail'),  # ty: ignore[invalid-route-handler]
        ]
        ",
        "
        def detail(request): ...
        ",
    );

    #[test]
    fn an_ignore_comment_on_the_route_silences_it() {
        assert_eq!(
            project(SILENCED.0, SILENCED.1).checked_python_diagnostics("blog/urls.py"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_ignore_comment_that_silenced_a_route_is_not_then_reported_unused() {
        // the scan itself answers before any comment is read, and it is the fold into
        // the type checker's pass that both silences the diagnostic and marks the
        // comment used. without the second half, `unused-ignore-comment` fires on the
        // very comment that did the silencing
        assert_eq!(
            check_urls(SILENCED.0, SILENCED.1),
            ["invalid-route-handler Error: `detail` takes no argument named `pk` [views.detail]"]
        );
        assert!(
            !project(SILENCED.0, SILENCED.1)
                .checked_python_diagnostics("blog/urls.py")
                .iter()
                .any(|diagnostic| diagnostic.contains("unused-ignore-comment"))
        );
    }
}
