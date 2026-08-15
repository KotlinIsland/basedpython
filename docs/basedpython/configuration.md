# configuration

a project is configured by a `basedpython.toml` at its root, or by the
`[tool.basedpython]` section of its `pyproject.toml`

```toml
# basedpython.toml
[run]
main = "app.cli"

[rules]
override-raise = "error"
```

the same settings in `pyproject.toml`, where every table is prefixed:

```toml
[tool.basedpython.run]
main = "app.cli"

[tool.basedpython.rules]
override-raise = "error"
```

a project has one configuration, not one per command: `by check`, `by run` and
`by build` all read the same options

## option groups

| group         | what it configures                                                               |
| ------------- | -------------------------------------------------------------------------------- |
| `environment` | the python version, platform, and where to find dependencies                     |
| `src`         | which files belong to the project (`include`, `exclude`, `respect-ignore-files`) |
| `rules`       | the severity of each diagnostic — `ignore`, `warn` or `error`                    |
| `analysis`    | how types are inferred, including the basedpython-only strictness switches       |
| `terminal`    | diagnostic output format, and whether warnings fail the run                      |
| `run`         | the project entry point for [`by run`](cli-reference.md#by-run)                  |
| `overrides`   | per-path variations of `rules` and `analysis`                                    |

individual options are documented with the feature they belong to — for example
[`analysis.sound-types`](features/sound-types.md),
[`analysis.precise-unsolved-typevars`](features/precise-unsolved-typevars.md) and
[`rules.override-raise`](features/exceptions.md#overrides). `by check --help`
lists the command line equivalents

## ty's names are read too

basedpython is built on [ty](https://github.com/astral-sh/ty), and ty's own names
hold exactly the same options: a `ty.toml` is read like a `basedpython.toml`, and
a `[tool.ty]` section like `[tool.basedpython]`. an existing ty project needs no
migration

where both appear, basedpython's name wins:

- a `basedpython.toml` supersedes a `ty.toml` in the same directory — the whole
    file, not option by option. the ignored file is named in a warning
- within one `pyproject.toml`, `[tool.basedpython]` beats `[tool.ty]` option by
    option, so the two sections can be mixed

## precedence

for a given project, highest precedence first:

1. command line options — `--python-version`, `--error`, `--config KEY=VALUE`
1. the file given to `--config-file`, which replaces the project's own configuration
1. the project's `basedpython.toml`, else its `ty.toml`, else its `pyproject.toml` sections
1. the [user-level configuration](#user-level-configuration)

a configuration file supersedes the `pyproject.toml` *sections*, but the
`[project]` table is still read — the project keeps its name, and a
`requires-python` lower bound still supplies the python version when
`environment.python-version` is unset

## project discovery

the project root is the closest ancestor directory of the checked path that has
a `basedpython.toml`, a `ty.toml`, or a `pyproject.toml` with a
`[tool.basedpython]` or `[tool.ty]` section. failing that it is the closest
directory with any `pyproject.toml`, and failing that the path itself, checked
with default options

a nested package with its own configuration is therefore its own project, and is
not governed by the configuration above it

## user-level configuration

a `basedpython.toml` in the config directory applies to every project:

| platform     | path                                     |
| ------------ | ---------------------------------------- |
| linux, macos | `~/.config/basedpython/basedpython.toml` |
| windows      | `%APPDATA%\basedpython\basedpython.toml` |

`$XDG_CONFIG_HOME` is honoured where it is set, and `ty/ty.toml` in the same
directory is read as a fallback. any project setting beats it

## per-path overrides

an override applies `rules` and `analysis` settings to the files it matches,
which is how a strictness option is adopted one directory at a time:

```toml
[[overrides]]
include = ["src/core/**"]

[overrides.analysis]
sound-types = true
```

`include` defaults to everything and `exclude` to nothing; within one override
`exclude` wins. later overrides beat earlier ones, and all of them beat the
top-level `rules` and `analysis`

## per-file configuration

a [PEP 723](https://peps.python.org/pep-0723/) script carries its own
configuration, which applies to that file alone:

```by
# /// script
# [tool.basedpython.rules]
# division-by-zero = "ignore"
# ///

print(4 / 0)
```

the block is a layer, not a replacement: every rule it does not mention is
whatever the project says. it beats the project's top-level `rules` and
`analysis`, and loses to an `[[overrides]]` entry that matches the file and to
anything given on the command line
