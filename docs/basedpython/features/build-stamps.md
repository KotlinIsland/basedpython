# build stamps

a `build:` block declares values the build settles when it produces the
artifact. each one is read as an attribute of `build`, at the type it declares:

```by
build:
    GIT_SHA: str

def main():
    print(build.GIT_SHA)
```

```python
class build:
    GIT_SHA: str = "e6f9ac1d4b2a7c3f9081be2d5a4c7e13f8b0d6a2"
def main():
    print(build.GIT_SHA)
if __name__ == "__main__":
    main()
```

the value is fixed for the life of the artifact — every run of that build
reports the same commit — and `build.GIT_SHA` is a `str` to the checker, so a
misspelled stamp is an error rather than an attribute that is missing at
runtime

the feature is [experimental](../configuration.md#experimental-features), so a
project asks for it by name:

```toml
# basedpython.toml
[experimental]
build-stamps = true
```

a block written without opting in is an error rather than a no-op. it still
lowers, so a program that reads a stamp keeps working — which is why nothing at
the point of use would tell you the value was never settled

## what the build supplies

| stamp            | type   | is                                                 |
| ---------------- | ------ | -------------------------------------------------- |
| `GIT_SHA`        | `str`  | the commit `HEAD` names                            |
| `GIT_SHA_SHORT`  | `str`  | its first twelve characters                        |
| `GIT_BRANCH`     | `str`  | the branch checked out                             |
| `GIT_TAG`        | `str`  | the tag *on this commit*, empty when there is none |
| `GIT_DIRTY`      | `bool` | whether the tree had uncommitted changes           |
| `BUILT_AT`       | `str`  | when the build ran, RFC 3339 in UTC                |
| `PYTHON_VERSION` | `str`  | the python the output was lowered to               |

declare only what the program uses. a stamp the block does not name is not
computed into anything

`GIT_DIRTY` is worth declaring wherever `GIT_SHA` is. a build from a tree with
uncommitted changes is not the commit it names, and a program that cannot say so
will eventually be asked to explain a traceback that does not match its source:

```by
build:
    GIT_SHA_SHORT: str
    GIT_DIRTY: bool

def version() -> str:
    return build.GIT_SHA_SHORT + ("-dirty" if build.GIT_DIRTY else "")
```

## a stamp the build cannot supply

a stamp with no default is a claim that the build has to produce it. when it
cannot — no `git` on the machine, or a source tree that is not a checkout, as an
exported tarball is — the transpile fails and says which stamp:

```text
the build supplied no value for the stamp `GIT_SHA`, and it has no default
```

that failure is the point of writing the declaration down: nothing has to
remember to check, and no artifact goes out claiming a commit it does not have.
give the stamp a default where the program would rather carry on without it:

```by
build:
    GIT_SHA: str = "unreleased"
```

a default only stands in. when the build does supply a value, that value wins

## stamps a project settles itself

`--stamp NAME=VALUE` supplies one directly, and beats anything the build would
have worked out for itself — which is what a CI job that knows the commit it was
dispatched for wants, since the checkout it runs in may be shallow or headless:

```sh
by build --stamp GIT_SHA=$GITHUB_SHA --stamp BUILD_NUMBER=$GITHUB_RUN_NUMBER
```

```by
build:
    GIT_SHA: str
    BUILD_NUMBER: int
```

a stamp is written to the program as text, so it can be declared `str`, `int` or
`bool`. a `bool` reads the spellings a shell produces — `true`/`1`/`yes`/`on`
against `false`/`0`/`no`/`off`, and an empty value is `false`. a value that is
not the type it was declared with is an error, not a coercion:

```text
the stamp `RUN` is declared `int`, but the build supplied `later`, which is not one
```

## reproducible builds

`BUILT_AT` is the one stamp that differs between two builds of the same source.
when `SOURCE_DATE_EPOCH` is set — the ecosystem's agreement on what "now" is for
a build that has to come out the same twice — it is used instead of the clock,
so a project with a `BUILT_AT` stamp still produces a byte-identical artifact:

```sh
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct) by build
```

## the version is not a stamp

there is deliberately no built-in `VERSION`. a wheel already carries its version
in its metadata, and the authoritative way to read it is to ask for it:

```by
from importlib.metadata import version

def about() -> str:
    return version("app")
```

a stamped copy is a second answer that can disagree with the first — the wheel
says `1.4.0` and the program says `1.3.9` — and there is no way to tell from
inside which one is wrong. a project that wants one anyway can stamp it
explicitly with `--stamp VERSION=…`

## a wheel built from a source distribution

publishing runs the build twice: once to make the source distribution, and again
to make a wheel out of it — and the second one happens where there is no
checkout to read a commit from.

that is not a problem here. what a source distribution carries is already
transpiled python, and the pyproject in it names an ordinary python backend, so
building a wheel from it never transpiles anything a second time. the stamps the
first build settled are simply what is in the file. a `pip install` from source
gets the same commit the sdist was made from

## where the values come from

the build settles the stamps once and hands them to the transpiler; the
transpiler never goes looking for them. that is what keeps the emitted python a
function of its source — the same source and the same stamps give the same file,
every time, which is what lets one file be recomputed into a tree an earlier
build wrote and still agree with the modules around it about what commit they are

it also means the checker never sees a value. `by check` knows `build.GIT_SHA`
is a `str`; what that `str` holds is settled by whichever build runs

## stamps are not a place for secrets

a stamp is written into the artifact as a literal, so it is readable by anyone
who has the wheel — and a build that refuses one reports the value it was given,
which puts it in the build log too. that is the right behaviour for a commit
hash and the wrong place for a token. read a secret from the environment at
startup instead, where it stays out of both
