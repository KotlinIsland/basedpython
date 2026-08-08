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

**no diagnostics.** templates are never type-checked — an unknown variable or a misspelled filter is not reported, it just doesn't complete.

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

the editor usually says so: `django`, `django-html`, `django-txt`, `htmldjango` and `jinja-html` are all taken as django templates.

when the editor just calls the file `html` — which is what plain vs code does — the path decides: a file with a template-ish extension (`.html`, `.htm`, `.txt`, `.xml`, `.django`, `.dj`, `.jinja`) somewhere under a directory named `templates` is a django template. ordinary html elsewhere in the project is left alone.

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

a project whose settings cannot be read still has its own `templatetags` modules and django's builtin tables, and behaves exactly as it did before.

## limitations

**the context has to be written where it is passed.** the names are read out of the dict literal in the `render()` call or the view class, so a context built somewhere else and passed by name — `return render(request, "…", context)` — contributes nothing.

**a member django refuses to call is still offered.** django won't call `save()` or `delete()` from a template — they carry `alters_data` — but `django-stubs` doesn't record that, so there is nothing to read it from. they appear in the list, below the model's own fields.

**dictionary lookups don't resolve.** django tries a subscript before an attribute, so `{{ mapping.key }}` works at runtime, but a mapping's keys are values rather than types and completions can only offer attributes.

**route names are read by walking the url tree from `ROOT_URLCONF`.** that is where the namespaces come from — the `namespace=` an `include()` gives, or the included module's own `app_name` — and it is what reaches an installed package's urlconf, so `include('rest_framework.urls')` contributes `rest_framework:login` like any route of the project's own. a project whose settings don't say where the tree starts falls back to reading each of its modules on its own: an include's namespace is unknown then, and nothing outside the project is reached.

**a route is named by a `path()` or by a rest framework router.** `router.register('books', BookViewSet, basename='book')` names `book-list` and `book-detail`, and each `@action` on the viewset names one more. a `DefaultRouter` names `api-root` besides, where a `SimpleRouter` names none. a registration whose basename cannot be worked out — no `basename=` and no resolvable `queryset` — names nothing rather than guessing.

**a name written in python has to be a literal.** the string in a `render()` or a `reverse()` is read as it is written, so a name built from an f-string, or held in a variable, is just a string.

## see also

- [django support](django.md) — the type checker's understanding of models, fields and queries
- [django template language](https://docs.djangoproject.com/en/stable/ref/templates/language/)
- [django-template-partials](https://github.com/carltongibson/django-template-partials) — where `{% partialdef %}` came from before django 6.0
