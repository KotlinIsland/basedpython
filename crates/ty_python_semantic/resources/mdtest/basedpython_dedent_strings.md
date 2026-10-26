# dedented triple-quoted strings

A lone triple-quoted string that opens with a newline and closes on a line of its own is dedented:
the transpiler strips the indent its lines share, and drops the opening newline and the line the
closing quotes sit on. Its value is the text that is left, so that is the literal we infer.

## the value is the dedented text

```by
def f():
    text = """
        hello
          world
        """
    reveal_type(text)  # revealed: "hello\n  world"
```

A blank line keeps only its newline, however much whitespace it was written with.

```by
text = """
    a

    b
    """
reveal_type(text)  # revealed: "a\n\nb"
```

Escapes are read from the dedented text, and a raw string keeps its backslashes.

```by
escaped = """
    \t(\
    x
    """
reveal_type(escaped)  # revealed: "\t(x"

raw = r"""
    \d+
    """
reveal_type(raw)  # revealed: "\\d+"
```

## f-strings

The literal text of an f-string is dedented the same way.

```by
def f(name: str):
    # revealed: "hello\n  there"
    reveal_type(f"""
        hello
          there
        """)
```

## t-strings

```toml
[environment]
python-version = "3.14"
```

So is the literal text of a t-string: its `strings` are read from the dedented text.

```by
from string.templatelib import Template

def greet(name: str) -> Template:
    return t"""
        hello {name}
          there
        """

assert greet("you").strings == ("hello ", "\n  there")
```

## strings that are not dedented

A string written on the line of its opening quotes, a string whose lines share no indent, a bytes
literal and a string written as several adjacent literals are all left as written.

```by
inline = """one line"""
reveal_type(inline)  # revealed: "one line"

unindented = """
a
"""
reveal_type(unindented)  # revealed: "\na\n"

data = b"""
    a
    """
reveal_type(data)  # revealed: b"\n    a\n    "

joined = """
    a
    """ "b"
reveal_type(joined)  # revealed: "\n    a\n    b"
```

## strings a dedent cannot read

The closing quotes set the depth the content is written at, so content written shallower than them
has no consistent dedent, and we reject the string.

```by
# error: [invalid-syntax] "is indented more than the content of the triple-quoted string"
text = """
  a
    """
```

A `\` ending the last line would continue the string past the newline before the closing quotes,
which is the newline the dedent drops, so we reject that too. A raw string has no line continuation,
and keeps its backslash.

```by
# error: [invalid-syntax] "a line continuation cannot end the last line of a dedented string, whose newline the dedent drops"
text = """
    a\
    """

# error: [invalid-syntax]
formatted = f"""
    a\
    """

raw = r"""
    a\
    """
reveal_type(raw)  # revealed: "a\\"
```

## a dedented name in `__all__`

`__all__` holds what the program does, so a name written in a dedented string is exported.

`exporter.by`:

```by
def f(): ...
def g(): ...

__all__ = ["""
    f
    """]
```

```by
from exporter import *

f()
g()  # error: [unresolved-reference]
```
