# language injection

a string can say what language is written inside it, and an editor then treats
it as that language — highlighting it, checking it, completing in it

```by
# language=javascript
script = "const total = items.length"
```

the marker is a comment on the statement above, which is how editors have
spelled this for years. the language is whatever you write after `language=`,
and nothing in basedpython interprets it: your editor matches it against the
languages it has, so `sql`, `html`, `json` and `basedpython` all work as well
as your editor supports them

the marker can trail the statement instead, at the end of its last line:

```by
query = "select name from users"  # language=sql
```

a comment trailing one statement is that statement's alone — the statement on the
next line is not marked by it

## a regular expression is one already

what python's `re` functions take as their pattern — `re.compile`, `re.match`,
`re.search`, `re.sub`, `re.split` and the rest — is written in python's regular
expression syntax, and is reported as `PythonRegExp` with nothing written to say so:

```by
import re

word = re.compile(r"(?P<word>\w+)")   # PythonRegExp
```

which function is called is resolved, not read off the spelling, so
`from re import sub as replace` is found as well, and a `re.compile` of some other
module is not. a pattern handed in through a parameter that passes it straight on
travels the way [a declared language does](#the-language-travels-to-the-call).
`PythonRegExp` is what PyCharm calls python's dialect of regular expressions; an
editor with only a generic regular expression language reads it as that

## a parameter can say it instead

a function that takes source code says so once, on the parameter, rather than
at every call:

```by
from typing import Annotated

def evaluate(source: Annotated[str, "language=javascript"]): ...

evaluate("const total = items.length")
```

the metadata is read by tools and ignored by the type checker — `source` is an
ordinary `str` parameter, and marking one cannot make working code stop
checking

## the language travels to the call

a string handed to a parameter that passes it straight on is the same string,
one call further out, so the language reaches it there too:

```by
def evaluate_twice(source: str):
    evaluate(source)
    evaluate(source)

evaluate_twice("const total = items.length")   # javascript, by way of `evaluate`
```

this only follows a parameter that is handed on untouched. a body that assigns
the name, or that uses it from a nested function, stops it — putting a language
on a string that is not written in it would report ordinary text as broken
code, so the doubtful cases report nothing

## what an editor gets

for a language your editor knows, its own support for that language takes over
inside the string: its parser, its inspections, its completion, its
refactorings. none of that comes from basedpython

for `basedpython` itself the fragment is checked as what it is — a module —
so an error inside the string is an error, underlined under the characters that
caused it:

```by
# language=basedpython
snippet = """
    x: int = "no"
    """
```

a fragment is checked on its own, so it sees the standard library but not the
names around the string it sits in

a pattern handed to one of python's `re` functions is checked by basedpython
itself: its `invalid-regex` rule compiles it as `re` would, where the pattern is
a literal and the call's flags say whether it is verbose, and reports what `re`
would refuse. `by/injections` marks each such fragment
`"checked": true`, whatever decided its language, so an editor whose own regular
expression support reports errors too can leave them to basedpython there, and
report its own everywhere else. with `invalid-regex` turned off, nothing is
marked

the indentation a triple-quoted string
[strips](dedent-strings.md) is stripped from the fragment too, which is what
makes an indented block of basedpython or python a program rather than a syntax
error

## what is not injected

a string carrying an escape is left alone. the fragment is the characters as
written, so `"\n"` would reach it as a backslash and an `n` rather than as a
newline — write the fragment raw or triple-quoted instead

an f-string is left alone too. it is a string with holes in it, and the text
between the holes is not a program in any language

a fragment written as several adjacent literals is one fragment:

```by
# language=sql
query = "select name" " from users"
```

such a fragment keeps its indentation, because a string written this way is not
[dedented](dedent-strings.md) either. write it as one triple-quoted string to
have the indentation stripped
