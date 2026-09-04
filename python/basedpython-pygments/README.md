# basedpython-pygments

a [pygments](https://pygments.org) lexer for basedpython, so that ```` ```by ```` code
blocks in the documentation are syntax highlighted

pygments picks the lexer up from an entry point, so nothing needs to reference it —
installing the package is enough. it is pulled into the `basedpython-docs` dependency group of the
repository root, which is what the docs build installs

```sh
uv sync --group basedpython-docs --no-install-project
uv run --no-sync zensical serve
```

## keeping it honest

basedpython's added keywords are soft: `get`, `data` and `out` are all still ordinary
identifiers, and the real parser tells them apart by position. the lexer approximates
that with per-keyword lookaheads, so it can drift from the language

`scripts/check_by_lexer.py` runs the lexer over every `by` block in `docs/basedpython`
and fails if a block produces an error token, or if a keyword the docs demonstrate does
not come out as a keyword. it runs in `prek`
