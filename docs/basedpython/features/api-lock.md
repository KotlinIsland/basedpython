# api lockfile (`api.lock`)

`by generate-api-file` produces a deterministic, line-oriented summary of
the public type-level surface of a project. the file is meant to be
committed alongside the source and reviewed as a diff: any change to the
public api shows up as a line-level change in the lockfile

```sh
by generate-api-file               # writes api.lock in the project root
by generate-api-file --stdout      # writes to stdout
by generate-api-file -o public.lock
```

the project is the one the current directory is in, or the one `--project`
names, and its lockfile is `api.lock` in its root directory wherever the command
runs from

the lockfile is regenerated for the python its `#python:` header names, so
`by generate-api-file --python-version 3.10` once and `by generate-api-file`
after that keep it for 3.10. `--python-version` names another, and with no
lockfile, or one with no header, it is generated for the version the project is
checked against. the command fails where the lockfile cannot be read, or names a
python version `by` cannot read, rather than guess what it is for

## workflow

- commit `api.lock` to version control next to `pyproject.toml`
- in CI, regenerate the lockfile and fail if `git diff --exit-code api.lock`
    is non-empty
- during code review, the diff is the api change. a reviewer who
    approves the diff has approved the api change

## in an editor

the lockfile is generated code: any edit to the source can change it, not only a
merge. the language server (`by server`) generates it from the project as the
editor has it, unsaved edits included, and incrementally, so an editor can keep
it in step without running the command

- `by/apiLock` takes `{"uri"?: "…"}`, a file or directory of the project
    whose lockfile is asked for, the project the server analyses that file in
    (the first project's without one), and answers
    `{"uri", "text", "python", "exists", "staleLine"}`: where the project keeps
    its lockfile, the lockfile as the server generates it now, the target that
    lockfile is generated for as its `#python:` header names it, whether the
    project has an `api.lock`, and the line, 0-based, at which that `api.lock`
    first says something else, `null` where it is up to date or there is none.
    it answers `null` without a project, and fails where the project's
    `api.lock` cannot be read, not being utf-8 among other reasons, or names a
    python version `by` cannot read
- an `api.lock` the editor opens in the server, whatever language it calls it,
    is answered when it pulls diagnostics for it (`textDocument/diagnostic`):
    where it is stale, one warning, `stale-api-lock`, on the first line that
    differs. the server says that diagnostics depend on other files, so an
    editor asks again after an edit anywhere
- `textDocument/codeAction` answers that diagnostic with the quick fix
    *Regenerate api.lock*, an edit rewriting the whole file as the server
    generates it

in each, the lockfile is generated for the target the project's `api.lock`
names, `#python:3.10` for one written by
`by generate-api-file --python-version 3.10`, so a lockfile kept for the lowest
python a library supports is checked and regenerated for that python; with no
`api.lock`, or `#python:default`, it is the version the project is checked
against. it is the same text the command writes for that target, byte for byte

### what makes a lockfile stale

every line counts except `#tool:by=…`, which names the `by` that wrote the
file. two versions of `by` that write the same records describe the same api: if
the line counted, every upgrade of `by` would make every lockfile stale, and a
project worked on with two versions of `by` would never be up to date, with no
change to its api. the format line, the target and the module count do count.
line endings do not, since a checkout may have turned them into `\r\n`

## record grammar

the file opens with four `#`-prefixed header lines: the format version
(`#api-lock:v=1`), the generating `by` version (`#tool:by=0.0.5`), the target
(`#python:3.13`, or `#python:default` when no `--python-version` was given) and
the number of modules walked (`#modules:12`). every line after them is one
record. fields are colon-separated, and the records are sorted lexicographically

```text
<qualified>:c[<bases>]                       # class
<qualified>:c<tv>[<bases>]                   # generic class
<qualified>:c[<bases>]{<flags>}              # class with flags
<qualified>:d(<params>)-><ret>               # def / method
<qualified>:d{<deco>}<tv>(<params>)-><ret>   # def with decorators / typevars
<qualified>:v=<type>                         # variable / class attribute
<qualified>:v[<quals>]=<type>                # qualified variable
<qualified>:i=<type>                         # instance attribute (via self.x)
<qualified>:t=<type>                         # type alias
<qualified>:t<tv>=<type>                     # generic type alias
<qualified>:p=getter|setter|deleter,...      # property accessors
<qualified>:r=<target>                       # re-export of class/function/alias
<qualified>:m=<module>                       # module re-export
```

`<qualified>` is the dotted path from the project root: `pkg.mod.ClassName.method`

`<tv>` is `<variance? name, ...>` — `+T` covariant, `-T` contravariant,
`*Ts` typevar-tuple, plain `T` invariant

`<flags>` joins on `,`: `dataclass`, `enum`, `final`, `named_tuple`,
`protocol`, `typed_dict`

`<deco>` joins on `,`: `abstractmethod`, `async`, `classmethod`,
`deprecated`, `final`, `no_type_check`, `overload`, `override`, `property`,
`staticmethod`, `type_check_only`

`<quals>` joins on `,`: `classvar`, `final`, `initvar`, `notrequired`,
`readonly`, `required`

## what's included

- module-level functions, classes, type aliases, variables, and re-exports
    whose name does not start with `_`
- instance attributes assigned via `self.x = ...` inside `__init__`
- dunders conventionally part of the public surface (`__all__`,
    `__author__`, `__doc__`, `__version__`)

## what's excluded

- any symbol whose simple name starts with `_` (unless it's one of the
    conventional public dunders above)
- stdlib, site-packages, and other non-first-party modules
- output from `by build` and `by compile`, which a build marks with a
    `.by-manifest`: it is not part of the project, whatever the directory is
    called. a package of the project's own called `build` is described like any
    other

## determinism and stability

lockfile lines are sorted lexicographically and union members are written
in a canonical order, so two runs against the same source produce
byte-identical output

## python-version sensitivity

`by generate-api-file --python-version 3.10` and `--python-version 3.13`
can produce materially different lockfiles for the same source, because
many typing constructs (`Self`, `Required`, `NotRequired`, `LiteralString`,
…) are only resolvable on newer targets. for libraries that support a
range of python versions, pick the lowest supported version as the
canonical lockfile target
