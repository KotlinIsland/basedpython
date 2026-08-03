# api lockfile (`api.lock`)

`by generate-api-file` produces a deterministic, line-oriented summary of
the public type-level surface of a project. the file is meant to be
committed alongside the source and reviewed as a diff: any change to the
public api shows up as a line-level change in the lockfile

```sh
by generate-api-file               # writes ./api.lock
by generate-api-file --stdout      # writes to stdout
by generate-api-file -o public.lock
```

## workflow

- commit `api.lock` to version control next to `pyproject.toml`
- in CI, regenerate the lockfile and fail if `git diff --exit-code api.lock`
    is non-empty
- during code review, the diff is the api change. a reviewer who
    approves the diff has approved the api change

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
- output from `by build` — the `out/` directory is not considered first-party
    source for lockfile purposes

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
