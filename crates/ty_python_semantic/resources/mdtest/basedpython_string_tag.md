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

## a tag resolves against a block's receiver

A [trailing lambda](basedpython_trailing_lambda.md) block puts its receiver's members in scope
unqualified, and a tag is an ordinary name in that scope — so `text"…"` reaches the receiver's
`text` exactly as a written-out `text(...)` call does.

```toml
[environment]
python-version = "3.14"
```

```by
from string.templatelib import Template

class Tag:
    def text(self, t: Template) -> int:
        return 1

    def div(self, block: Tag.() -> None) -> None:
        block(self)

def build(root: Tag, who: str) -> None:
    root.div:
        reveal_type(text"hello {who}")  # revealed: int
```

## an attribute carries a tag

The tag may be reached through an attribute, which is the spelling that survives a local of the
same name shadowing it.

```toml
[environment]
python-version = "3.14"
```

```by
from string.templatelib import Template

class Doc:
    def text(self, t: Template) -> int:
        return 1

def build(doc: Doc, who: str) -> None:
    reveal_type(doc.text"hello {who}")  # revealed: int
```

## an attribute tag that does not exist is an error

The attribute is looked up like any other name.

```toml
[environment]
python-version = "3.14"
```

```by
class Doc:
    pass

def build(doc: Doc) -> None:
    # error: [unresolved-attribute]
    doc.missing"oops"
```
