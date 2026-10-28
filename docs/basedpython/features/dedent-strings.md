# dedented triple-quoted strings

triple-quoted strings that open with a newline and have a consistently
indented body are auto-dedented at transpile time, so source indentation
does not leak into the runtime string value:

```by
def doc():
    return """
        hello
          world
        """
```

the logical content is `"hello\n  world"` — the common leading indent of
each interior line is stripped

that is the value the checker sees too: the literal type of `doc()`'s string is
`"hello\n  world"`, a regular expression written this way is checked as the
pattern `re` will compile, and a name written this way in `__all__` is the name
exported

## trigger conditions

dedenting fires when *all* of these hold:

- the string is triple-quoted (`"""` or `'''`)
- the opening quote is immediately followed by a newline (`"""\n`)
- the closing quotes sit on a line of their own, after nothing but whitespace

inline triple-quoted strings (`"""one line"""`) and strings without a
newline after the opening quote are left untouched. the rule applies
to plain strings, raw (`r"""..."""`), f-strings (`f"""..."""`) and
t-strings (`t"""..."""`). a bytes literal is never dedented, and nor is a
string written beside another in an implicit concatenation

## closing-quote indent rule

the closing `"""` must be indented **no more** than the content's common
leading indent. otherwise the rewrite has no consistent dedent depth and
the transpiler raises an error, and the checker reports a syntax error:

```by
text = """
  asdf
    """
```

→ `closing """ is indented more than the content of the triple-quoted string`

equal indent is fine (content fully dedents). less indent is also fine
(content keeps the surplus). this rule only flags the genuinely misaligned
case

## relationship to PEP 750 / `textwrap.dedent`

there is no runtime call, and the result is not what `textwrap.dedent` would
produce: the opening newline and the line the closing quotes sit on are dropped,
so `textwrap.dedent("\n    a\n    ")` is `"\na\n"` and the same text written
this way is `"a"`. a `\` that continues a line joins the next line after that
line's indent has been stripped. the rewrite preserves embedded f-string
interpolations:

```by
greet = f"""
    hello {name}
    """
# → f"""\
# hello {name}\
# """
```

a raw string holds its backslashes, so a line continuation can't drop its
opening and closing newlines: it is written as the plain string that holds the
same text instead, with the backslashes and quotes of its literal text escaped

```by
pattern = r"""
    \d+ "units"
    """
# → """\
# \\d+ \"units\"\
# """
```

a `\` ending the last line of the content of a string that is not raw is
rejected, since it would continue onto the newline the dedent drops
