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
| `{% url '‸' %}`     | the project's route names, namespaced by `app_name`                                                                   |
| `{% static '‸' %}`  | the files under the project's `static` directories                                                                    |
| `{% load ‸ %}`      | the libraries not yet loaded, django's and the project's                                                              |
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

## limitations

**no diagnostics.** templates are never type-checked. an unknown variable or a misspelled filter is not reported — it just doesn't complete.

**a `TEMPLATES` setting is not read.** template directories are found by name rather than from `settings.py`, so a `DIRS` entry pointing somewhere else is not picked up.

**the context has to be written where it is passed.** the names are read out of the dict literal in the `render()` call or the view class, so a context built somewhere else and passed by name — `return render(request, "…", context)` — contributes nothing.

**a member django refuses to call is still offered.** django won't call `save()` or `delete()` from a template — they carry `alters_data` — but `django-stubs` doesn't record that, so there is nothing to read it from. they appear in the list, below the model's own fields.

**dictionary lookups don't resolve.** django tries a subscript before an attribute, so `{{ mapping.key }}` works at runtime, but a mapping's keys are values rather than types and completions can only offer attributes.

**`{% url %}` namespaces come from `app_name`.** a namespace given at the `include()` instead is not applied.

## see also

- [django support](django.md) — the type checker's understanding of models, fields and queries
- [django template language](https://docs.djangoproject.com/en/stable/ref/templates/language/)
- [django-template-partials](https://github.com/carltongibson/django-template-partials) — where `{% partialdef %}` came from before django 6.0
