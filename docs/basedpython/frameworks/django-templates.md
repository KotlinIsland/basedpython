# django templates

basedpython's language server understands django template files, not just the python around them. templates get syntax-aware highlighting, completions and go-to-definition, and all of it is joined up with the project's own django definitions — the models, views, urls and template tag libraries the type checker already knows about.

## what the editor gets

### completions

what is offered depends on where the caret is.

| position            | what is offered                                                                                                       |
| ------------------- | --------------------------------------------------------------------------------------------------------------------- |
| `{% ‸ %}`           | tag names: django's builtins, the project's own registered tags, and the `{% end… %}` that closes the block you're in |
| `{{ x\|‸ }}`        | filter names: django's builtins and the project's own registered filters                                              |
| `{{ ‸ }}`           | the variables the view puts in this template's context, plus the names the template itself binds                      |
| `{{ book.‸ }}`      | the attributes of `book`'s type — for a django model its own fields lead, ahead of what `models.Model` brings         |
| `{% extends '‸' %}` | the project's template paths                                                                                          |
| `{% include '‸' %}` | the project's template paths                                                                                          |
| `{% url '‸' %}`     | the project's route names, under the namespaces its `include()`s put them in                                          |
| `{% static '‸' %}`  | the files under the project's `static` directories                                                                    |
| `{% load ‸ %}`      | the libraries not yet loaded: django's, the installed apps', and the project's                                        |
| `{% block ‸ %}`     | the blocks the parent template defines and this one hasn't overridden yet                                             |
| `{% partial ‸ %}`   | the fragments `{% partialdef %}` declares                                                                             |

the tag that closes the block you're inside is always offered first, so `{% for %}` … `{%` completes to `{% endfor %}` in one keystroke, with `{% empty %}` right behind it.

a tag or filter from a library the template hasn't loaded comes with the `{% load %}` it needs as an extra edit, written below the `{% extends %}` if there is one.

`{% partialdef %}` and `{% partial %}` are django's own from 6.0 and need no `{% load %}`. on an older django they come from [django-template-partials](https://github.com/carltongibson/django-template-partials), which does need one — the tags are offered either way, but the load is not written for you.

### go-to-definition

| on                                    | goes to                                                             |
| ------------------------------------- | ------------------------------------------------------------------- |
| `{% extends 'blog/base.html' %}`      | that template                                                       |
| `{% include 'blog/card.html#item' %}` | that template, or the `{% partialdef item %}` inside it             |
| `{% url 'blog:detail' %}`             | the `path(…, name='detail')` that names the route                   |
| `{% partial card %}`                  | the `{% partialdef card %}` that declares it                        |
| `{% block content %}`                 | the block it overrides, in the parent template                      |
| `{% load blog_extras %}`              | the tag library module                                              |
| a custom tag or filter name           | the python function registered under it                             |
| `{{ book }}`                          | the context entry the view supplies, or the tag that binds the name |
| `{{ book.title }}`                    | the type `title` is read off — for a model field, the model class   |

### semantic highlighting

the editor's own html grammar keeps the markup; the server adds what a grammar cannot know:

- a tag or filter that is django's own is marked `defaultLibrary`, so the project's own tags are visibly distinct from the builtins
- a name is a variable, an attribute of one, or a keyword argument, and each is coloured differently
- the name a `{% for %}` or `{% with %}` binds is marked as a definition where it is bound
- `{% block %}` and `{% partialdef %}` names read as the fragments they name, not as variables
- `{% comment %}` bodies and `{# … #}` are comments; `{% verbatim %}` bodies are plain text

### find references

asking for a name's references answers the reverse of every navigation above: from a `{% block %}`, every template in the family that declares it — including, from a *base* template, every child that overrides it, which is the question you have while editing one. from anywhere in a template, everything that renders it. from a route name, every `{% url %}` and `reverse()`. from a tag or filter, every template that uses it and the `@register` that declares it.

a use a rename could not rewrite is still reported here: knowing where a name is written is useful even where it cannot be changed for you.

### hover and outline

hovering says what the thing under the caret is: a variable or a path segment as its type, a tag or a filter as its documentation and the `{% load %}` it needs, a `{% block %}` as the block it overrides, an `{% extends %}` as the file it resolves to, a `{% url %}` as its route pattern.

a template's `{% block %}`s and `{% partialdef %}`s are its outline, nested as they nest, and every block tag folds.

### signature help

a filter's argument is the one thing a template writes that nothing beside it explains. with the caret after the `:`, `{{ value|date:"‸" }}` says what the filter takes: the registered function's second parameter, read from the project's own filters and from django's own wherever django itself can be read, together with what the filter is documented to do.

a filter that takes no argument offers nothing, which is the answer to having typed a `:` after one.

### inlay hints

a `{% for book in shelf %}` shows the element type it binds, which is what says what `{{ book.… }}` will offer. an `{% include %}` and an `{% extends %}` show the file the name resolves to, which matters because two apps can ship the same template name and only one of them is loaded.

**diagnostics.** a template reports twelve rules — an unclosed block, an unknown tag or filter, a tag used without its `{% load %}`, a missing template or static file, an unknown route or the wrong arguments to one, a block no ancestor declares, and a member django cannot call. each is silent unless the index behind it is authoritative, so a project whose settings cannot be read reports nothing rather than guessing. silence one with `{# ty: ignore #}` or `{# ty: ignore[rule-name] #}`, or configure it under `[tool.ty.rules]` like any other.

a comment that silenced nothing is reported as `unused-ignore-comment`, the same rule a stale `# ty: ignore` raises in a python file, so a suppression left behind by a fix doesn't sit there unnoticed. the one thing a template adds is that a check reporting nothing because its index was not authoritative has decided nothing, and a comment naming that rule is left alone rather than called unused.

`by check` reports them too. a template is not in the project's python file set and is never read as python — it is checked as the template it is — but it is part of the project, so the command line and the editor say the same thing about the same file. only the project's own templates are checked; an installed app's are a dependency's source, and a project with no django has no django templates.

an unknown *variable* is still not reported: a context can come from places a scan cannot see, so absence is not evidence.

### the project's django structure

a workspace symbol search answers with the django project as well as with the python in it. searching for a name offers the models, the admin classes, the views a route reaches, the route names, the templates by the name their loader uses, the `{% partialdef %}`s, and the tags and filters the project and its installed apps register — each labelled with what django calls it, beside whatever python contributed under the same name.

a `{% block %}` is deliberately not among them. a block name is chosen against the one template it overrides rather than to be unique in a project, so a search for `content` would answer with one entry per template that declares one. a `{% partialdef %}` is the opposite: it exists to be rendered from elsewhere by name.

## the same names, written in python

a template name in a `render()` and a route name in a `reverse()` are plain strings as far as python is concerned. the editor reads them as the names they are, so the same completions and the same navigation are there from the python side.

| position                              | what is offered              | goes to                  |
| ------------------------------------- | ---------------------------- | ------------------------ |
| `render(request, '‸')`                | the project's template paths | that template            |
| `TemplateResponse(request, '‸')`      | the project's template paths | that template            |
| `template_name = '‸'` in a view class | the project's template paths | that template            |
| `reverse('‸')`, `reverse_lazy('‸')`   | the project's route names    | whatever names the route |
| `redirect('‸')`                       | the project's route names    | whatever names the route |

the function is matched by its last segment, so `shortcuts.render` and `render` are one, and `render()` accepts its template by keyword as `template_name=` too.

`redirect()` takes a url or a model as readily as a route name, so a string with a `/` in it is left alone entirely and a name the url configuration doesn't have leads nowhere.

### a route and the view it names

a route pattern names its arguments and django hands each of them to the view by keyword. the two are checked against each other:

```py
path("books/<int:pk>/", views.detail, name="detail")

def detail(request, pk: int): ...   # correct
def detail(request, id: int): ...   # invalid-route-handler: the route names `pk`
def detail(request): ...            # invalid-route-handler: `pk` has nowhere to go
def detail(request, pk: str): ...   # invalid-route-parameter-type: the converter gives an `int`
```

the first two are a `TypeError` when the url is requested; the last is a value that arrives with a type its annotation denies.

what a route gives its view includes what every pattern it is mounted behind names, so an `include('blog.urls')` written under `books/<int:pk>/` puts `pk` in front of every view below it. the check therefore runs only where the url tree could be walked in full.

nothing is reported for a view that could accept more than it appears to: one taking `*args` or `**kwargs`, one reached through a decorator the type checker cannot see through, one nothing can resolve to a definition, or a route passing extra arguments of its own through `path()`'s third argument. an argument matched by a `re_path()` group goes through no converter, so its name is checked and its type never is.

a class-based view is reached through `as_view()`, which django calls rather than the class — so what has to take the route's arguments is `get`, `post`, `dispatch` and the rest. only the ones the class declares itself are checked: every handler django ships takes `**kwargs`, so an inherited one accepts anything.

these two read the whole url tree rather than the one file, which is not something the type checker's own pass can do, but they arrive with what it found: `by check` and the editor both report them, and a `# ty: ignore[invalid-route-handler]` on the route silences one and counts as used, exactly as it would for a type error.

## how a template's variables are found

the chain runs from the template back to the type checker:

```by
# blog/views.py
def post(request):
    book = Book.objects.get(pk=1)
    return render(request, "blog/post.html", {"book": book})
```

```django
{# blog/templates/blog/post.html #}
{{ book.author.name }}
```

the server finds the `render()` call that names `blog/post.html`, reads `book` out of its context dict, asks the type checker what that expression is — a `Book` — and then walks `author` and `name` through the model's fields. django's field machinery is already understood (see [django support](django.md)), so a `ForeignKey` traverses and a `null=True` field is optional, exactly as in python.

a lookup that lands on something callable is called, exactly as django calls it, so the idioms that traverse through a method resolve:

```django
{{ book.get_absolute_url }}                       {# `str`, not the method #}
{% for c in book.author.book_set.all %}           {# a `Book`, through the manager #}
    {{ c.title }}
{% endfor %}
```

a method that needs an argument is not called — django renders nothing for it, and neither does the server offer anything past it.

django refuses outright to call a method marked `alters_data`, and renders nothing instead of writing to the database from a template. those resolve to nothing too, and are reported:

```django
{{ book.save }}                {# warning: django will not call `save` from a template #}
{{ book.author.book_set.create }}
```

the ones it marks are `save`, `delete` and their `async` twins on a model, and the `create`/`update`/`bulk_*` family on a queryset, a manager and the manager behind a relation. overriding one keeps the mark, so an override is reported too.

going the other way, a member marked `do_not_call_in_templates` is used as it is rather than called — django's `Choices` classes carry it, so `{{ Color.RED }}` is the member and not whatever calling it would give.

the same works through the template's own bindings:

```django
{% for entry in shelf %}
    {{ entry.title }}      {# `entry` is one element of `shelf` #}
{% endfor %}

{% with writer=book.author %}
    {{ writer.name }}      {# `writer` is `book.author` #}
{% endwith %}
```

these are the places a context is read from:

- `render(request, "…", {…})` and `TemplateResponse(request, "…", {…})`, positionally or by keyword
- a class-based view's `template_name`, together with its `context_object_name`, its `extra_context`, and the `context["…"] = …` writes in its `get_context_data`

two views rendering the same template both contribute; a name they agree on appears once.

## how a template is recognised

the editor usually says so: `django`, `django-html`, `django-txt` and `htmldjango` are all taken as django templates.

when the editor just calls the file `html` — which is what plain vs code does — the path decides: a file with a template-ish extension (`.html`, `.htm`, `.txt`, `.xml`, `.django`, `.dj`) somewhere under a directory named `templates` is a django template. ordinary html elsewhere in the project is left alone.

jinja is not django, and nothing here is claimed for it. a document the editor calls `jinja`, `jinja-html` or `jinja2` gets no template services at all, and neither does a `.jinja` file — everything below answers as django, so a correct `{% set %}`, `{% macro %}` or `|default("x")` would be reported as an error.

template directories, static directories and tag libraries are found by their conventional names, which is what django's own app-directories loaders use:

- `**/templates/**` — templates, named by their path below `templates/`
- `**/static/**` — static files, named by their path below `static/`
- `**/templatetags/*.py` — tag libraries, named by their module

## how the settings are read

the module `manage.py` points `DJANGO_SETTINGS_MODULE` at is read as well, and what it says is added to what the convention already found. a settings module that can't be found, or a value in one that can't be worked out, costs only itself.

| setting                               | what it adds                                                            |
| ------------------------------------- | ----------------------------------------------------------------------- |
| `TEMPLATES[*]["DIRS"]`                | template directories that need not be called `templates`                |
| `TEMPLATES[*]["APP_DIRS"]`            | which of two same-named templates django's loader reaches first         |
| `TEMPLATES[*]["OPTIONS"]["builtins"]` | libraries every template has loaded already                             |
| `STATICFILES_DIRS`                    | static directories that need not be called `static`                     |
| `INSTALLED_APPS`                      | the apps searched, in the order they are searched — and their libraries |

### tag libraries from the installed apps

each installed app's `templatetags` package is scanned the way django's own `get_installed_libraries` walks them, so `{% load humanize %}` completes and `|intcomma` is offered once `django.contrib.humanize` is installed. django's own `django.templatetags` is always a candidate alongside them.

only the apps `INSTALLED_APPS` names are looked at — site-packages at large is never searched, so an app that isn't installed contributes nothing, exactly as at render time.

a library in `TEMPLATES[*]["OPTIONS"]["builtins"]` is available in every template without a `{% load %}`, so none is suggested for it and none is written.

### django's own tags and filters

django's implicit builtins — `django.template.defaulttags`, `django.template.defaultfilters` and `django.template.loader_tags`, the modules every engine starts with loaded — are scanned the same way. so `{% for %}`, `{% extends %}` and `|upper` are read off the installed django rather than assumed.

that is what decides which of django's names exist and which library each comes from. a tag this django registers is offered whether or not it is a name we already knew, and a tag we knew that this django does not register is not offered as django's — a `{% partialdef %}` is a builtin in django 6.0 and needed `{% load partials %}` before it, and it is the installed django that says which.

there is a hardcoded table of django's tags and filters behind that, and it supplies what a scan cannot see: which tag closes which block, which tags may appear in between, and the documentation shown in completion and hover.

a project whose settings cannot be read, or whose django cannot be resolved at all, has its own `templatetags` modules and that table, and behaves exactly as it did before.

## limitations

**the context has to be written where it is passed.** the names are read out of the dict literal in the `render()` call or the view class, so a context built somewhere else and passed by name — `return render(request, "…", context)` — contributes nothing.

**a member django refuses to call is still offered, struck through.** it is genuinely there, and someone reading `{{ book.save }}` is better served by being told what it is than by being shown nothing, so it is marked and sorted below every member django will render rather than hidden.

**`alters_data` is django's own set rather than something read off the code.** django writes the mark on the function object, which no stub can express and `django-stubs` declares nowhere — grepping the package finds nothing. so the names come from django's source, and only apply to django's own classes: a class of the project's own with a `save` method is not reported, and neither is a method a model adds that django has no equivalent of.

**dictionary lookups don't resolve.** django tries a subscript before an attribute, so `{{ mapping.key }}` works at runtime, but a mapping's keys are values rather than types and completions can only offer attributes.

**route names are read by walking the url tree from `ROOT_URLCONF`.** that is where the namespaces come from — the `namespace=` an `include()` gives, or the included module's own `app_name` — and it is what reaches an installed package's urlconf, so `include('rest_framework.urls')` contributes `rest_framework:login` like any route of the project's own. a project whose settings don't say where the tree starts falls back to reading each of its modules on its own: an include's namespace is unknown then, and nothing outside the project is reached.

**a route is named by a `path()` or by a rest framework router.** `router.register('books', BookViewSet, basename='book')` names `book-list` and `book-detail`, and each `@action` on the viewset names one more. a `DefaultRouter` names `api-root` besides, where a `SimpleRouter` names none. a registration whose basename cannot be worked out — no `basename=` and no resolvable `queryset` — names nothing rather than guessing.

**a name written in python has to be a literal.** the string in a `render()` or a `reverse()` is read as it is written, so a name built from an f-string, or held in a variable, is just a string.

**only a route the configuration names is paired with its view.** the route index is built out of the names a `{% url %}` reverses, so a `path("books/<int:pk>/", views.detail)` written without a `name=` is not checked against `views.detail`.

**neither `--fix` nor `--add-ignore` touches a template.** both rewrite a file through its python tokens, and a template has none. a template diagnostic is reported unchanged by either, and a `{# ty: ignore #}` has to be written by hand — or taken from the quick fix the editor offers.

## see also

- [django support](django.md) — the type checker's understanding of models, fields and queries
- [django template language](https://docs.djangoproject.com/en/stable/ref/templates/language/)
- [django-template-partials](https://github.com/carltongibson/django-template-partials) — where `{% partialdef %}` came from before django 6.0
