# acknowledgements

third-party work basedpython relies on but does not include

the root [`LICENSE`](https://github.com/KotlinIsland/basedpython/blob/main/LICENSE)
covers the libraries basedpython is *derived* from — code vendored into this
repository. the entries below are different: nothing here is copied,
redistributed, or linked. they are packages that transpiled basedpython output
imports at runtime, and that the person running that output installs
themselves. they are acknowledged here because relying on someone's work is
worth saying out loud, even when no licence obliges it

## regex

|            |                                                        |
| ---------- | ------------------------------------------------------ |
| package    | [`regex`](https://pypi.org/project/regex/)             |
| author     | Matthew Barnett                                        |
| repository | [mrab-regex](https://github.com/mrabarnett/mrab-regex) |
| licence    | `Apache-2.0 AND CNRI-Python`                           |

the [grapheme string surface](features/character.md) — `count`, `first`,
`last`, `characters`, `character_at`, `reversed`, `prefix`, `suffix` — is
grapheme-correct, and grapheme correctness needs an engine that implements
[UAX #29](https://unicode.org/reports/tr29/). `regex` is the only widely
available python engine that does, via its `\X` escape, so the lowerings for
those accessors emit `import regex` and it becomes a runtime dependency of any
program that uses them. the standard library's `re` has no `\X`, and splitting
on code points instead would silently miscount every multi-code-point grapheme
— five for `"🤦🏼‍♂️"` rather than one

the two licences apply to different parts of the package: `CNRI-Python` to the
original `re` code derived from CPython (copyright 1998-2001 Secret Labs AB),
`Apache-2.0` to Matthew Barnett's additions and alterations (copyright 2020).
both are permissive, and basedpython neither vendors nor redistributes any of
it — `import regex` in generated output is an ordinary dependency of the
generated program, resolved from the user's own environment
