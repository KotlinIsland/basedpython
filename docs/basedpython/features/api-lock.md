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
    has diagnostics like any other document, which the editor pulls
    (`textDocument/diagnostic`) or the server publishes
    (`textDocument/publishDiagnostics`): where it is stale, one warning,
    `stale-api-lock`, on the first line that differs, which says so where that
    is the format line of a lockfile in another format, and where it cannot be
    checked, one warning, `unreadable-api-lock`, on the `#python:` header naming
    a python version `by` cannot read. an edit to a module changes them, so 200
    ms after the last of a run of edits to a python or basedpython document, or
    after one is closed, which reads it from disk again, while another document is
    open, the server sends a client that supports it
    `workspace/diagnostic/refresh`, since not every editor acts on the
    `interFileDependencies` the server declares, and publishes again the
    diagnostics of every document the client does not pull
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
the number of modules walked (`#modules:12`), where a module with a stub
beside its source is one module, read from the stub as an import of it is.
every line after them is one
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
<qualified>:r=<target>                       # another name for a class/function/alias
<qualified>:m=<module>                       # module re-export
```

`<qualified>` is a path: a module's dotted name from the project root, then
the names within it, `pkg.mod.ClassName.method`. a class, function or type
alias defined inside a function has a path through it the way python's
`__qualname__` spells one, `pkg.mod.make.<locals>.Made`

`<target>` names what an `r` record is another name for: the path of a class,
function or type alias this file describes, or of one outside the project in its
own module (`builtins.dict`, `dep.Outer.Inner`), followed for a specialized
class by its type arguments, `[<type>,...]`

`<tv>` is `<variance? name, ...>` — `+T` covariant, `-T` contravariant,
`*Ts` typevar-tuple, plain `T` invariant

`<flags>` joins on `,`: `dataclass`, `enum`, `final`, `named_tuple`,
`protocol`, `typed_dict`

`<deco>` joins on `,`: `abstractmethod`, `async`, `classmethod`,
`deprecated`, `final`, `no_type_check`, `overload`, `override`, `property`,
`staticmethod`, `type_check_only`

`<quals>` joins on `,`: `classvar`, `final`, `initvar`, `notrequired`,
`readonly`, `required`

## where a definition is described

a class, function or type alias is described once, by its own record and, for
a class, the records of its members, at the path of the name its definition
binds. every other path that reaches it is an `r` record naming that path

```python
# shapes.py
class Shape:
    class Corner: ...

    def area(self) -> int: ...

class Box[T]: ...

Corner = Shape.Corner
IntBox = Box[int]
area = Shape.area
```

```text
shapes.Box:c<+T>[]
shapes.Corner:r=shapes.Shape.Corner
shapes.IntBox:r=shapes.Box[builtins.int]
shapes.Shape.Corner:c[]
shapes.Shape.area:d(self:shapes.Shape)->builtins.int
shapes.Shape:c[]
shapes.area:r=shapes.Shape.area
```

this holds whichever module reaches it: an attribute of a class in another
module that holds `Shape` is `r=shapes.Shape`, and `shapes.Shape` is described
in `shapes` all the same

a definition the public api reaches only under another name, because its own
name is private, left out of `__all__` or local to a function, is still
described at its own path, so `Public = _Impl` writes `Public:r=m._Impl` and
describes `m._Impl`. a class a call makes, such as `Q = NamedTuple("P", ...)`,
is described at the name it is bound to, `m.Q`, and one bound to no name is
described wherever it is reached

a name's value is what a module importing it reads: what the name is declared as
at the end of its scope, or else what it is bound to. a class statement declares
its name, so `A = 1` after `class A` leaves `m.A` the class, as the checker reads
it. two class statements of one name declare both classes, and both are
described at that path

something from outside the project is named wherever the project reaches it,
in a module or a class: `mapping = dict[str, int]` in a class is
`r=builtins.dict[builtins.str,builtins.int]`

## what's included

- module-level functions, classes, type aliases, variables, and re-exports
    whose name does not start with `_`
- instance attributes assigned via `self.x = ...` inside `__init__`
- dunders conventionally part of the public surface (`__all__`,
    `__author__`, `__doc__`, `__version__`)

## what's excluded

- any symbol whose simple name starts with `_` (unless it's one of the
    conventional public dunders above, or a definition the public api reaches
    under another name)
- stdlib, site-packages, and other non-first-party modules
- output from `by build` and `by compile`, which a build marks with a
    `.by-manifest`: it is not part of the project, whatever the directory is
    called. a package of the project's own called `build` is described like any
    other

## determinism and stability

lockfile lines are sorted lexicographically and union members are written
in a canonical order, so two runs against the same source produce
byte-identical output. what is written for a path depends only on the project,
never on the order its modules are read in

## format versions

`#api-lock:v=` names the version of the record grammar and of the rules for
where records go. a lockfile in another format than the `by` reading it writes
is regenerated in its own by `by generate-api-file`, which says so, and the
editor warns on the format line until it is regenerated

## python-version sensitivity

`by generate-api-file --python-version 3.10` and `--python-version 3.13`
can produce materially different lockfiles for the same source, because
many typing constructs (`Self`, `Required`, `NotRequired`, `LiteralString`,
…) are only resolvable on newer targets. for libraries that support a
range of python versions, pick the lowest supported version as the
canonical lockfile target
