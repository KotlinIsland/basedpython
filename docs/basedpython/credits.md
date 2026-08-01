# credits

- [kotlinisland](https://github.com/KotlinIsland) - author and maintainer
- [Joren Hammudoglu](https://github.com/jorenham) - design work
- [charliecloudberry](https://github.com/charliecloudberry) - technical writer
- [detachhead](https://github.com/detachhead) - design work
- Chloe Assouline - graphic design

## upstream

basedpython is a fork of [ruff](https://github.com/astral-sh/ruff), and its
type checker is built on [ty](https://github.com/astral-sh/ty). the parser, the
AST, the fix-application machinery and the whole checking core are the work of
the [Astral](https://astral.sh) team and the wider ruff community — basedpython
adds a language on top of tools that were already excellent

every ruff and ty contributor is credited in
[the upstream repository's history](https://github.com/astral-sh/ruff/graphs/contributors);
their copyright is carried in the root
[`LICENSE`](https://github.com/KotlinIsland/basedpython/blob/main/LICENSE)

## third-party runtime dependencies

these are packages that transpiled basedpython output
imports at runtime, and that the person running that output installs
themselves

### regex

|            |                                                        |
| ---------- | ------------------------------------------------------ |
| package    | [`regex`](https://pypi.org/project/regex/)             |
| author     | Matthew Barnett                                        |
| repository | [mrab-regex](https://github.com/mrabarnett/mrab-regex) |
| licence    | `Apache-2.0 AND CNRI-Python`                           |

used for [`Character` and grapheme support](features/character.md) — its `\X`
escape implements [UAX #29](https://unicode.org/reports/tr29/), which the
standard library's `re` has no equivalent for

the two licences apply to different parts of the package: `CNRI-Python` to the
original `re` code derived from CPython (copyright 1998-2001 Secret Labs AB),
`Apache-2.0` to Matthew Barnett's additions and alterations (copyright 2020).
both are permissive, and basedpython neither vendors nor redistributes any of
it — `import regex` in generated output is an ordinary dependency of the
generated program, resolved from the user's own environment
