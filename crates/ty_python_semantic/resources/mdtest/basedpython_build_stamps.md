# build stamps

a `build:` block declares the values the build settles when it produces the artifact — the commit it
came from, whether that commit's tree was clean, when it happened. each one is read as an attribute
of `build`, at the type it was declared with

(every stamp here carries a default, so that each example stands on its own without a build behind
it. whether a stamp has one changes what happens at build time, never its type.)

the feature is experimental, so a project asks for it by name:

```toml
[experimental]
build-stamps = true
```

## a stamp has the type it declares

```by
build:
    GIT_SHA: str = "unreleased"
    GIT_DIRTY: bool = False
    BUILD_NUMBER: int = 0

reveal_type(build.GIT_SHA)  # revealed: str
reveal_type(build.GIT_DIRTY)  # revealed: bool
reveal_type(build.BUILD_NUMBER)  # revealed: int
```

## a default does not narrow the stamp

the default stands in when the build supplies nothing, so it says what the value falls back to and
not what it is. a stamp annotated `str` is a `str` whichever of the two it ends up holding — it is
not the `Literal` the default would infer as on its own

```by
build:
    VERSION: str = "0.0.0+dev"

reveal_type(build.VERSION)  # revealed: str
```

## a stamp nothing declared is not there

the block is the whole list of what the program stamps, so reaching for anything else is a mistake
the checker can catch — unlike an environment variable read through a mapping, which can only fail
at runtime

```by
build:
    GIT_SHA: str = "unreleased"

build.GIT_BRANCH  # error: [unresolved-attribute]
```

## stamps are ordinary values

nothing about a stamp is special once it is read: it flows into anything its type fits, and is
rejected by anything it does not

```by
build:
    GIT_SHA: str = "unreleased"
    GIT_DIRTY: bool = False

def describe(sha: str, dirty: bool) -> str:
    return f"{sha}{'-dirty' if dirty else ''}"

describe(build.GIT_SHA, build.GIT_DIRTY)

# error: [invalid-argument-type] "Argument to function `describe` is incorrect: Expected `str`, found `bool`"
describe(build.GIT_DIRTY, True)
```

## `build` is only a keyword in front of a block

a name is not taken away by a declaration form that does not use it. `build` reads as an ordinary
identifier everywhere the block header is not

```by
build = 3
reveal_type(build)  # revealed: 3
```

## the feature is off unless the project asks for it

a block written while the feature is off is reported rather than ignored. the block still lowers, so
a program that reads a stamp keeps working — which is exactly why nothing at the point of use would
say the value was never settled

```toml
[experimental]
build-stamps = false
```

```by
# error: [invalid-build-stamps] "`build` is an experimental feature, and is off"
build:
    GIT_SHA: str = "unreleased"

reveal_type(build.GIT_SHA)  # revealed: str
```

## a nested block is reported too

the lowering fills a block in wherever it is written, so one nested inside a class is a stamp the
same way a module-level one is

```toml
[experimental]
build-stamps = false
```

```by
class Program:
    # error: [invalid-build-stamps] "`build` is an experimental feature, and is off"
    build:
        GIT_SHA: str = "unreleased"
```
