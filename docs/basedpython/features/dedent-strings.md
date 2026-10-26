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

## trigger conditions

dedenting fires when *all* of these hold:

- the string is triple-quoted (`"""` or `'''`)
- the opening quote is immediately followed by a newline (`"""\n`)
- the final line before the closing quote is whitespace-only

inline triple-quoted strings (`"""one line"""`) and strings without a
newline after the opening quote are left untouched. the rule applies
uniformly to plain strings, raw (`r"""..."""`), f-strings (`f"""..."""`),
t-strings (`t"""..."""`), and prefixed combinations

## closing-quote indent rule

the closing `"""` must be indented **no more** than the content's common
leading indent. otherwise the rewrite has no consistent dedent depth and
the transpiler raises an error:

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

the resulting string is identical to what `textwrap.dedent` would produce,
but there is no runtime call. the rewrite preserves embedded f-string
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
