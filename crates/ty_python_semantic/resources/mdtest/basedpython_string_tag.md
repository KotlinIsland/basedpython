# basedpython: custom string tags

a custom string tag is an identifier placed directly before a string literal with no intervening
whitespace. `tag"..."` lowers to `tag(t"...")`, a call passing the literal as a
[PEP 750](https://peps.python.org/pep-0750/) `Template`. the tag is any in-scope callable with a
`(Template) -> T` signature, so ty infers the result from the tag's return type with no special
handling

```toml
[environment]
python-version = "3.14"
```

## result type is the tag's return type

```by
from string.templatelib import Template

class Query: ...

def sql(template: Template) -> Query:
    return Query()

q = sql"select * from t"
reveal_type(q)  # revealed: Query
```

## interpolating tag has the same result type

```by
from string.templatelib import Template

class Html: ...

def html(template: Template) -> Html:
    return Html()

name = "world"
page = html"<p>{name}</p>"
reveal_type(page)  # revealed: Html
```

## the tag receives a Template

a tag whose parameter is annotated `Template` accepts the lowered argument without a diagnostic

```by
from string.templatelib import Template

def render(template: Template) -> str:
    return ""

reveal_type(render"hi {1}")  # revealed: str
```

## the argument is type-checked

the lowered call is checked like any other, so a tag whose parameter cannot accept a `Template`
reports an `invalid-argument-type` — there is no special-casing that bypasses argument checking

```by
def takes_int(n: int) -> int:
    return n

# error: [invalid-argument-type]
bad = takes_int"oops"
```

## a non-callable tag is an error

a tag must resolve to something callable; using a non-callable name is a normal call error

```by
not_callable = 1

# error: [call-non-callable]
x = not_callable"oops"
```

## an undefined tag is an error

```by
# error: [unresolved-reference]
y = undefined_tag"oops"
```
