# editor support

the `by` language server backs the editor experience: completions, inlay
hints, an outline, and the rest of the language-server protocol. this page
covers the parts that are basedpython's own — the ones you would not find by
guessing from python

## postfix templates

writing `.` after an expression offers a set of templates that rewrite the
whole expression rather than reading as attribute access. accepting `.print`
on `xs` replaces `xs.print` with `print(xs)`:

```by
xs.print        # → print(xs)
```

| template                            | rewrites `x` into                                     | offered for               |
| ----------------------------------- | ----------------------------------------------------- | ------------------------- |
| `.print`                            | `print(x)`                                            |                           |
| `.not`                              | `not x`                                               |                           |
| `.type` `.repr` `.str` `.len`       | `type(x)`, `repr(x)`, `str(x)`, `len(x)`              | `.len`: what has a length |
| `.list` `.set` `.tuple` `.dict`     | `list(x)`, `set(x)`, `tuple(x)`, `dict(x)`            |                           |
| `.isinstance`                       | `isinstance(x, object)`                               |                           |
| `.par`                              | `(x)`                                                 |                           |
| `.compl` `.comps` `.compg` `.compd` | a list, set, generator or dict comprehension over `x` | what can be iterated      |
| `.return` `.raise` `.yield`         | `return x`, `raise x`, `yield x`                      |                           |
| `.if` `.while`                      | an `if` or a `while` on `x`                           |                           |
| `.ifn` `.ifnn`                      | `if x is None:`, `if x is not None:`                  | what may be `None`        |
| `.with`                             | `with x as name:`                                     | a context manager         |
| `.for` `.iter`                      | a `for` loop over `x`                                 | what can be iterated      |
| `.fore` `.itere` `.enum`            | `for index, item in enumerate(x):`                    | what can be iterated      |
| `.match`                            | a `match`                                             |                           |
| `.let` `.var`                       | a binding                                             |                           |

these are PyCharm's python templates, under its names, less `cast` and
`main`. one that needs something of `x` is offered only where `x`'s type has
it, and on an expression whose type says nothing, any. the templates from
`.return` down are statements, so they are only offered where the expression is
the whole statement — `f(xs.print)` offers `.print` but not `.for`. the ones
that open a suite or need a name to bind place the cursor for you, when your
editor supports snippets

each is a completion labelled with its name, `for`, and the detail `postfix`,
whose `textEdit` replaces the whole `x.for` and whose `filterText` is that text

`.let` and `.var` spell basedpython bindings; the rest are valid python and are
offered in `.py` files too

### postfix `await`

[`.await`](await-attribute.md) is not a template — it is real syntax, so it
completes as an ordinary word. it appears inside an `async def`, on an
expression that can actually be awaited

## completions

every completion carries a `textEdit` saying what it replaces: what is typed of
the name before the cursor, `car` in `f"{car`, all of a string literal before
the cursor, `slow-mo` in `"slow-mo`, and more where a completion stands for more,
as a postfix template does for the expression it rewrites. so an editor never
works out the range from its own idea of a word, which inside an f-string is the
whole string token, against which nothing would match. what is typed is matched
against the name, so a completion labelled with more than its name carries the
name as its `filterText`: `inspect.walktree`, which `completions.importStyle`
`module` writes, matches `walktre`

### smart completion

an editor's smart, type-matching completion asks `by/smartCompletion`, which
takes what `textDocument/completion` takes and answers a completion list of the
same items, kept to those that fit the type the position expects: an argument
expects its parameter's type, a `return` what its function declares it returns,
the value of a declared assignment the declared type, and a `case` pattern the
subject's. a value fits when its type is assignable to the expected one, and a
class or a function when calling it makes such a value, so on `draw(` for a
`draw(shape: Shape)` the list is the variables holding a `Shape`, `Shape` and
its subclasses, and the functions returning one. a completion of type `Any` or
`Unknown`, a function that never returns and a keyword argument's name fit
nothing for certain, and nothing is imported. of nested positions the innermost
decides: in `x: int = len(` it is `len`'s parameter. the answer is `null` where
the position expects no type the source states, or `Any`

### the entry point

at module level, `main` completes to the whole [entry point](main-function.md)
definition, with `async main` beside it. once the module defines one, the name
completes to it like any other

### keywords

every keyword a statement may open with is offered wherever it is valid, whether
it is spelled with one word or several. the single words are basedpython's own —
`extension`, `let`, `var`, and `init` inside a class body — and a construct
spelled with more than one keyword is offered whole: `async def`, `data class`,
`frozen data class`, `enum class`, `override def`, `static var`, and the rest of
the [modifiers](modifiers.md). the method modifiers only appear inside a class
body, and `async for` / `async with` only inside an `async def`

they are offered at the start of a statement only — after `async` the plain
`def` keyword completes the rest

### overriding

in a class body, every superclass member the class does not define is offered
as the whole header an override would be written with:

```by
class B(A):
    override def greet(self, name: str) -> str:
```

`object`'s members are left out

the name of a `def` written in a class body completes the same way, as the rest
of the header: `def gre` offers `greet(self, name: str) -> str:`. in a `.by`
file, taking it also writes `override` before the `def`, unless it is there
already

### types

a type position offers the words basedpython spells types with — `literal`,
`final`, `dynamic`, `some`, `protocol`, `typeof` — and drops the statement
keywords, which never read there. a [type parameter](generics.md) offers its
modifiers before the name: `in`, `out`, `in out`, `overlapping`, `reified`

### common aliases

a name that is conventionally an alias of a module completes as that module,
in a file that has not imported it yet. `np.` offers what numpy has, and
accepting one of those completions writes the import that binds the name:

```py
import numpy as np

np.arange
```

the alias itself completes the same way — typing `n` offers `np`, and taking it
writes `import numpy as np`

only a module the project actually has is offered, and only where the name is
free: a file that binds `np` to something of its own means that, and gets
nothing from numpy

the aliases are the ones the python ecosystem already writes — `np`, `pd`,
`plt`, `dt`, and the rest. a project spells aliases of its own in its
configuration, keyed by the alias:

```toml
[tool.ty.editor.common-aliases]
npt = "numpy.typing"
```

an alias configured under a name ty already knows replaces it

a name left unimported reports as unresolved, and the quick fix on that
diagnostic writes the same import

these are auto-imports, so turning off `ty.completions.autoImport` turns them
off too

### unimported names

a name the file has not imported is offered for import only when what you have
typed starts where one of its words does — at its start, after a `_`, a `.` or
any other character that is not a letter or a digit, at an upper-case letter that
follows a lower-case one, at the last of a run of capitals that a lower-case letter
follows (`set` offers `HTTPServer`), or at a letter that follows a digit (`sum`
offers `md5sum`) — and the rest follows in order. `ret` offers `reveal_type` and `NoReturn` to import, but not
`secrets` or `urlretrieve`, which merely contain the letters, and which would
otherwise crowd out `return` and the names in scope

a name the file has not imported still completes past the dot. `Asdf.` offers
the members of the `Asdf` an import would bind, and accepting one writes that
import:

```py
from mod import Asdf

Asdf.name
```

a module reads the same way — `mod.` offers what `import mod` reaches — and a
longer chain follows every step, so `mod.Asdf.` lands on those same members. a
package's submodules are among what it offers, and a submodule is imported from
its package, since `import pkg.sub` binds `pkg` rather than the `sub` the file
wrote:

```py
from pkg import sub

sub.Asdf
```

a name that two modules each define of their own is two offers, one per module,
each carrying its own import. one class that several modules re-export is a
single offer, since the copies would differ only by an import you cannot see

a name the file already binds means what the file says it means and gets
nothing, and a chain that starts from anything but a name — `Asdf().` — has no
name for an import to bind. as with any auto-import, the name has to be one you
have begun to write: a bare `mod.` in a file that never imported `mod` offers
nothing, because every symbol spelled `mod` anywhere would qualify

like the aliases above, these are auto-imports, and turning off
`ty.completions.autoImport` turns them off too

how an auto-import brings in a name from a module the file imports nothing from yet
is `ty.completions.importStyle` (`completions.importStyle` in the initialization
options): `"from"`, the default, writes `from inspect import walktree` and
completes `walktree`; `"module"` writes `import inspect` and completes
`inspect.walktree`. a module the file already imports is imported from the way the
file does it, whichever is set — a name from a module imported with `import` is
written through it, and one from a module with a `from ... import` is added to that
statement. how imports are sorted, grouped and split is not an editor setting: it
is the project's isort configuration, which `buff` reads and fixes

an import any of these write — a completion's, an unresolved name's quick fix's, a
pasted name's, an inlay hint's — goes where `buff check --select I --fix` would put
it, so adding one never leaves the block unsorted. it joins its section of the file's first block of
imports (future, standard library, third party, first party, local, or the
sections the configuration defines) in the order isort sorts that section, with a
blank line around a section the block did not have yet, and a member added to an
existing `from` import is listed where isort orders the members. a new statement,
and a `from` import a new member makes too long, is wrapped to the line length as
isort wraps it, a section new to the block gets the heading `import-heading`
configures for it, and two members of a module the file does not import yet become
one `from` import:

```py
import json
import sys
from collections import OrderedDict

import attr
```

`math` is imported between `json` and `sys`, `Path` below the `collections`
import, `defaultdict` as `from collections import OrderedDict, defaultdict`, and
`attrs` below `attr`. the configuration is `buff`'s own, resolved as `buff`
resolves it — the closest `ruff.toml`, `.ruff.toml` or `pyproject.toml` with a
`[tool.ruff]` table, then the user's, then the defaults — so
`force-sort-within-sections`, `from-first`, `known-first-party`, `section-order`,
`no-lines-before`, `lines-between-types`, `force-single-line` and the rest apply
as they do to `buff`. the import has to run before the name it binds is used, so a
file whose imports all come after that point, or that has none, still gets it at
the top, below a docstring and any `from __future__` import, followed by the blank
lines isort writes after a block. `buff` sorts each cell
of a notebook on its own, so in a notebook the block an import joins, and the top it
otherwise goes at, are those of the cell the name is used in

### names inside a string

a name written inside a plain string's braces completes as a name, and taking
one turns the string into the f-string that reads it — the `f` and the closing
brace are written for you:

```py
name = "john"

"hello {na"     # → f"hello {name}"
```

the rest of the string is left exactly as it is written, so any other brace in
it starts meaning what an f-string reads it to mean once the prefix goes on

a docstring, a `case` pattern and a `str.format` template are not offered the
conversion, and neither is a string written where a type belongs: an f-string
is a different thing in each of those places, or no longer legal at all

### dict keys

the key of a subscript completes from what the dict is known to hold. a
`TypedDict`'s keys come from its type, and a plain dict's from the dict display
the name was bound to: after `d = {"alpha": 1, "beta": 2}`, `d["` offers
`alpha` and `beta`

### enum members and extensions

a bare [enum member](enums.md) is offered where the expected type admits one —
the value of a declared assignment, and a `case` pattern:

```by
a: Color = Red
```

attribute completions include the members any [`extension`](extensions.md)
block in scope declares on the receiver, alongside the type's own

### callables

accepting a completion for anything callable writes its parentheses and leaves
the caret between them, ready for the first argument — or after them, when the
callable takes no arguments at all, as `cart.total()` does for
`def total(self) -> int`. one whose parameters are all optional still leaves the
caret inside. set `ty.completions.completeFunctionParentheses` to `false` (or
`completions.completeFunctionParentheses` in the initialization options, for a
client that does not answer `workspace/configuration` with it) to insert the
bare name instead

## inlay hints

each kind of hint can be turned off on its own through the
`ty.inlayHints.<name>` setting your editor passes to the server. all default to
on

| setting                      | shows                                                           |
| ---------------------------- | --------------------------------------------------------------- |
| `variableTypes`              | the type of a variable the source does not annotate             |
| `callArgumentNames`          | the parameter each positional argument fills                    |
| `inferredRaises`             | the [exception set](exceptions.md) of an undeclared `def`       |
| `inferredVariance`           | the [variance](variance.md) inferred for a type parameter       |
| `inferredReification`        | `reified` on a parameter the body reifies                       |
| `inferredOverride`           | `override` on a method that overrides without saying so         |
| `inferredReads`              | the observables a `def` reads while composing (basedpython-ui)  |
| `parameterStability`         | `unstable` on a composable parameter the runtime cannot compare |
| `derivedDependencies`        | what a `derived(...)` computation depends on                    |
| `inferredInvalidations`      | the composables and `derived` values a state write re-runs      |
| `callTypeArguments`          | the type arguments inferred for a generic call                  |
| `typeArgumentNames`          | the parameter a positional type argument fills                  |
| `numericPromotions`          | the arms numeric promotion adds to `float` and `complex`        |
| `revealedTypes`              | what a `reveal_type` call reveals, and what it narrowed         |
| `implicitParameters`         | a [trailing lambda](trailing-lambdas.md)'s `it`                 |
| `implicitSelf`               | the `self` an [`init(...)`](init-method.md) binds               |
| `lambdaParameterTypes`       | the type of an unannotated `lambda` parameter                   |
| `inheritedParameterTypes`    | the type a parameter takes from the method it overrides         |
| `inheritedParameterDefaults` | the [default](inherited-defaults.md) it takes from that method  |
| `returnTypes`                | the return type of a `def` that leaves it out                   |
| `inferredParameterTypes`     | the `some` bound an unannotated parameter takes from its body   |
| `inferredBaseTypeArguments`  | the type arguments a generic base takes from its overrides      |
| `propertyTypes`              | the type a [property](properties.md) leaves to its accessors    |
| `implicitArguments`          | the [context arguments](context-parameters.md) a call fills     |
| `enumValues`                 | the value an [enum](enums.md) member takes implicitly           |
| `templateBindingTypes`       | a django template `{% for %}` binding's element type            |
| `resolvedTemplates`          | the file a django `{% extends %}` name resolves to              |

every hint also carries its setting's name as `data.kind` —
`{"kind": "revealedTypes"}` — so an editor can show each kind in its own way,
or only while a key is held, without telling kinds apart by their text. lsp's
own `kind` is only `Type` or `Parameter`, and a revealed `list[int]` and a
call's `[int]` differ by a bracket

a hint is shown or hidden by its own setting alone, but one setting also
changes what other hints say: in a `.by` file, `typeArgumentNames` names the
type arguments of the types that `variableTypes` and `callTypeArguments` hints
show, so `x = identity({"a": 1})` is hinted `: dict[Key=str, Value=int]` with it
on and `: dict[str, int]` with it off

### keeping a hand-aligned block aligned

a hint takes up room. drawn after the name, it pushes the rest of the line along
and a column of `=` the author lined up by hand stops being one:

```by
let a     = [1, 2]      # let a: list[int]     = [1, 2]
let basdf = 1           # unchanged: a hint for a bare literal is suppressed
```

the editor cannot repair that by narrowing the hint, because the padding the
author wrote can be narrower than the hint that displaced it. restoring the
column means widening the other lines too, and `basdf` is the line that has to
move — the line with no hint to hang the answer off. so the server answers which
lines were meant to be read together, with `by/alignmentGroups`:

```json
{
  "textDocument": { "uri": "file:///src/main.by" },
  "range": {
    "start": { "line": 0, "character": 0 },
    "end": { "line": 1, "character": 13 }
  },
  "tabSize": 4
}
```

`tabSize` is required. whether two `=` share a column depends on it as soon as a
tab comes before either, and it is the client's setting rather than anything the
source says, so there is no value the server could assume

the reply is the blocks whose lines the range reaches. `gapStart` is where the
padding before the `=` begins and `gapEnd` is the `=` itself, so the difference
between them is the room the author left:

```json
[
  {
    "members": [
      {
        "gapStart": { "line": 0, "character": 5 },
        "gapEnd": { "line": 0, "character": 10 },
        "gapStartColumn": 5,
        "gapEndColumn": 10
      },
      {
        "gapStart": { "line": 1, "character": 9 },
        "gapEnd": { "line": 1, "character": 10 },
        "gapStartColumn": 9,
        "gapEndColumn": 10
      }
    ]
  }
]
```

`gapStart` and `gapEnd` are positions, counted in the negotiated encoding's code
units. `gapStartColumn` and `gapEndColumn` are display columns, counted the way a
fixed-width editor lays the line out — a wide character takes two, a combining
mark none, and a tab reaches the next multiple of `tabSize`. those are the
numbers the grouping itself was decided by, and the ones to lay hints out in

a block is a run of assignments that are siblings in one suite, unseparated by a
blank line, already sharing a column, and padded — at least one of them written
with two or more spaces before its `=`. that last condition is what keeps
ordinary code out: `x = 1` over `y = 2` shares a column only because the names
are the same length, and padding those apart would be injecting space the author
never wrote

how wide anything ends up is the editor's to decide, because only the editor
knows which hints are on screen at a given instant — a kind can be switched off,
and push-to-hint draws a hint only while a key is held. what displaces a line is
every hint drawn on it at or before `gapEnd` added together, which is more than
one hint when the line unpacks: `a, b = 1, 2` is hinted after `a` and again
after `b`

a block is reported whole even when the range reaches only one of its lines,
since the column is sized against every line at once

## outline

a [property](properties.md) is one member in the source, so it is one entry in
the outline — not the getter, backing field and setter it lowers into. enum
variants and an extension's methods appear under their declarations

an attribute a method assigns on its receiver, `self.radius = radius`, is a
field of the class, listed after the members the class body declares. the first
assignment is its declaration, and a name the class body declares already is not
listed again

a `def` decorated `@property`, `@cached_property` or `@functools.cached_property`
is a property, and its `@diameter.setter` or `@diameter.deleter` is the same
property given another accessor, not a second member. each document symbol's
`detail` is what the source writes beside the name, on one line and printed
from the syntax tree rather than copied from the source: a function's
parameters and return annotation, `(self, radius: float) -> None`, and a class's
bases, `(Shape)`

### inherited members

a structure view that shows a class's inherited members asks
`by/documentInheritedMembers` with the document, once for all its classes. the
answer is every class of the outline, nested ones too, in source order, each by
the `selectionRange` of its name and with the `members` it inherits: what each
class of its method resolution order declares, as that class's outline has it,
attributes a method assigns on `self` among them, from the first class to
declare the name, and none the class declares itself. `object`'s are there,
last. each member is a document symbol of its superclass's file, with its
`name`, `kind`, `detail`, `range` and `selectionRange`, the `uri` of that file,
and the superclass as `containerName` and its module as `containerModule`:

```json
[
  {
    "selectionRange": { "start": { "line": 27, "character": 6 }, "end": { "line": 27, "character": 12 } },
    "members": [
      {
        "name": "describe", "kind": 6, "detail": "(self) -> str",
        "containerName": "Shape", "containerModule": "shapes.base",
        "uri": "file:///src/shapes/base.by",
        "range": { "start": { "line": 23, "character": 4 }, "end": { "line": 24, "character": 57 } },
        "selectionRange": { "start": { "line": 23, "character": 8 }, "end": { "line": 23, "character": 16 } }
      }
    ]
  }
]
```

the answer depends on every file the classes inherit from, and the server
announces no change to it: a client asks again after any document of the
workspace changes. it is `null` for a notebook and a template

## workspace symbols

`workspace/symbol` gives each python symbol the module and the classes it is
declared in as its `containerName`, `shapes.base.Circle` for the method `area`
of `Circle`, so that the three `area`s of a class hierarchy can be told apart.
the attributes a method assigns on `self` are found as fields of their class

a query with dots in it is a qualified name. its last part is matched against
the symbol's name and the parts before it against what the symbol is declared
in: `Shape.describe` finds `describe` in the class `Shape`, and `base.Sq` the
class `Square` of the module `shapes.base`. every part matches the way a whole
query does, as a subsequence of its letters. the last part before the name
matches what the symbol is directly in, and each part before that something
further out. a name with dots of its own, like a template's `base.html`, takes as
many of the query's parts as it has

## folding

`textDocument/foldingRange` gives every range a kind saying what it folds, so
that an editor can fold some kinds from the start, and the text to show in its
place when that says more than an ellipsis. the kinds LSP defines go to every
client; the others are basedpython's own, and go to a client that lists them in
`textDocument.foldingRange.foldingRangeKind.valueSet`

| kind         | folds                                                                     | folded to                    |
| ------------ | ------------------------------------------------------------------------- | ---------------------------- |
| `imports`    | a run of imports                                                          | `import ...`                 |
| `region`     | `# region <name>` through its `# endregion`                               | the name                     |
| `comment`    | a run of comments on lines of their own                                   |                              |
| `docstring`  | a module's, class's or function's docstring                               | `"""Its first line..."""`    |
| `function`   | a function's body, and its signature when that spans lines                |                              |
| `class`      | a class's body                                                            |                              |
| `string`     | any other string literal spanning lines                                   | `"""..."""`, with its prefix |
| `collection` | the inside of a list, tuple, set or dict display, or of a comprehension   |                              |
| `annotation` | a parameter's or variable's annotation, or what a `def` returns or raises |                              |

a client that does not list a kind of basedpython's own is told a docstring is a
`comment`, and is told no kind for the others. an annotation is the one range
that can lie on a single line: it is sent only to a client that lists the
`annotation` kind and does not set `lineFoldingOnly`, and not when it begins on the
line a block fold starts on, where it would put a second fold on that line. the text to fold to is sent
to a client that sets `foldingRange.collapsedText`. other blocks — an `if`, a
loop, a `with`, a `try` — and the arguments of a call fold with no kind

## refactorings

the server offers refactorings as code actions. each is worked out from what the
checker reads the file to mean, not from its text: inlining `x` replaces the reads
of that variable, never a same-named parameter of another function, an attribute
`obj.x` or a keyword argument `f(x=1)`

a refactoring either keeps the program meaning what it meant or is refused, and a
refusal says why. an editor that asks for refactorings explicitly shows the reason;
one that asks while the caret moves is only offered what applies

| refactoring             | kind                                   | offered on                           |
| ----------------------- | -------------------------------------- | ------------------------------------ |
| inline variable         | `refactor.inline.variable`             | a variable assigned once             |
| extract variable        | `refactor.extract.variable`            | a selected expression                |
| introduce constant      | `refactor.extract.constant`            | a selected expression of literals    |
| introduce parameter     | `refactor.extract.parameter`           | an expression of literals in a `def` |
| introduce attribute     | `refactor.extract.field`               | a selected expression in a method    |
| extract function        | `refactor.extract.function`            | selected statements                  |
| add return annotation   | `refactor.rewrite.returnAnnotation`    | a `def` header with no `->`          |
| convert to data class   | `refactor.rewrite.toDataClass`         | a class header                       |
| convert from data class | `refactor.rewrite.fromDataClass`       | a `data class` header                |
| put on separate lines   | `refactor.rewrite.splitList`           | a list not yet one element a line    |
| put on one line         | `refactor.rewrite.joinList`            | a list spread over several lines     |
| invert `if` condition   | `refactor.rewrite.invertIf`            | an `if` with an `else`               |
| apply De Morgan's law   | `refactor.rewrite.deMorgan`            | an `and` or an `or`                  |
| convert quotes          | `refactor.rewrite.flipQuotes`          | a string or bytes literal            |
| loop to comprehension   | `refactor.rewrite.loopToComprehension` | a loop filling an empty collection   |

an extraction makes up a name, which its user will want to choose. once its
action is resolved, its `data` says where the name is: `rename`, a `uri` and a
`range` in the document as the action's edit leaves it, at the place the
rewritten code uses the name, where the selection was. an editor applies the
edit and starts renaming there, as TypeScript's refactorings hand over a
rename location:

```json
{ "data": { "rename": { "uri": "file:///src/shapes.by", "range": { "start": { "line": 13, "character": 21 }, "end": { "line": 13, "character": 30 } } } } }
```

inlining is offered on the variable's assignment and on any of its reads alike, and
replaces every read either way. the value is parenthesised only where python
would read it otherwise: inlining `square = radius ** 2` into `math.pi * square`
writes `math.pi * radius ** 2`, and `x = a - b` into `c - x` writes
`c - (a - b)`

moving an expression can change when it runs, so that is what most refusals are
about. a value made of names and literals, and of python's own operators over
builtin numbers and strings — `'hello ' + name` of a `str` — is the same value
wherever it is evaluated, and is inlined into every read. any other value is only
inlined into a single read in the very next
statement, with nothing that could observe the move evaluated before it; an
expression is only extracted above its statement when it runs exactly once each
time the statement does — not in a loop condition, a conditional branch, a lambda
or after a call

```by
while queue.pending():  # extracting `queue.pending()` is refused: it runs every iteration
    queue.pop()
```

what could observe the move is asked of the checker rather than read off the
spelling. an operator of python's own over builtin numbers and strings — `+`, `-`
and `*` over numbers python does not convert to compute them, `+` joining strings,
the bitwise operators over integers, comparisons — runs no code of the program's
and cannot raise, and nor does reading a module's attribute. an operand counts as
a builtin number or string only where the checker knows it is exactly one: a
literal, a `final` type such as `final int`, or a `bool`. a plain `int` or `str`
may be an instance of a subclass, whose operators are the program's own, so a
declared `int` is not enough. so in `scale * math.pi * radius ** 2` the power can
be extracted where `scale` is a `final float` in a `.by` file. in python a `float`
also admits an `int`, and an `int` meeting a `float` or a `complex` is not taken
to be quiet, since one too large for it raises `OverflowError` (`10**400 + 0.5`);
nor is repeating a string, which raises it for a count too large for an index.
reading an attribute of an instance is not taken to be quiet, since it may be a
property; nor are `/`, `//`, `%` and `**`, which raise for some operands

a selection may be a run of operands that python groups otherwise: `a + b + c` is
`(a + b) + c`, so `b + c` is no expression of it, and extracting it evaluates
`a + (b + c)`. that is offered when the operator is associative over what the
operands are — `+`, `*` and the bitwise operators over integers, `+` over strings
or over byte strings — and not over floats, which round differently grouped the
other way

with nothing selected, a client that asks for `refactor.extract` kinds explicitly
(`triggerKind: Invoked` and `only` given) is offered an extraction for each
expression around the caret, innermost first, the same expressions
[type info](#type-info) lists less a bare name. each action's title names the
expression — `` Extract variable `value` from `len(items) + 1` `` — and its
`data.range` is the expression's range, for an editor that lets its user pick one
and highlights it; when none can be extracted, the innermost one's refusal is the
answer. a client asking as the caret moves is offered no extraction at a caret

adding a return annotation writes the return type the checker infers, importing
what it names. it is refused when that type is partly `Unknown` — `x + y` of two
unannotated parameters — since `Unknown` is no type a program can name, and the
rest of the union would claim more than the body says. an inlay hint's
double-click edit follows the same rule

introducing a parameter adds `name=expression` to the parameters of the function
the expression is in — last, or before a `**` parameter — and reads the parameter
where the expression was, so every call goes on passing what it passed. a default
is evaluated once, when the `def` runs, so this is offered only for what a constant
can be made of

introducing an attribute assigns `self.name = expression` above the statement in the
method and reads `self.name` where the expression was, by whatever name the method's
first parameter has. the name is one the class does not already have, its own or
inherited, and it is refused where an instance takes no new attribute: a class with
`__slots__` in its MRO, a frozen data class, an enum, a named tuple, a protocol or a
`TypedDict`, and in a class method or a static method — `__new__`,
`__init_subclass__` and `__class_getitem__` included — or a decorated method,
whose first parameter need not be the instance

extracting statements from a method makes a method of the same class, called
through the receiver. what the statements read from the function becomes a
parameter, and what they assign that is read afterwards is returned

a constant goes further than a variable: it is bound at the top of the module, so
it is evaluated as the module loads whether or not the expression would have been
evaluated at all. only an expression made of literals that cannot raise is
hoisted, and whether an operation over literals raises is asked of the checker
rather than read off its spelling — `60 * 60 * 24` is offered because ty folds it
to `86400`, while `1 / 0` and `1 + "a"` are not offered at all

a client that can resolve a code action's edit gets the action without one, and the
edit is computed when the action is chosen — against the document version it was
offered for, and refused as content modified if the document changed since. this
defers sending the edit, not working it out: whether a refactoring applies is
decided by working out the rewrite, so the actions a `textDocument/codeAction`
reply offers have each been computed to answer it. send `only` to narrow that —
a request that does not ask for `refactor` kinds does no refactoring work at all

inverting an `if` negates its condition and swaps its body with its `else`'s. a
negation takes off a `not`, swaps `and` and `or` by De Morgan's law, and flips a
comparison's operator only where that means the same: `is`, `is not`, `in` and
`not in` always, `==` and `!=` between python's own numbers, strings and bytes, and
the orderings between its integers, strings or bytes — `not x > 1` is not
`x <= 1` for a float that is `nan`, so a float's comparison is written
`not (x > 1)`. inverting an `if` and De Morgan's law are refused where a comment
in the condition would be lost. De Morgan's law rewrites `not (a or b)` as `not a and not b`, and
`a and b` as `not (not a or not b)` where only the truth of it is read — an `if`, a
`while`, an `assert` or a `not` — or where it is a `bool` already, since `a and b`
otherwise evaluates to `a` or `b`. converting a string's quotes escapes the new
quote where it appears and unescapes the old one, and is refused for a raw string
holding the new quote, which cannot be escaped. a loop that only adds to a
collection made empty right above it — a list's `append`, a set's `add`, a dict's
item assignment, under at most one `if` — becomes a comprehension, refused where
the loop's variable is read anywhere outside the loop that the loop's binding may
reach — above it on a later round, in a function, or as `del` (a comprehension
leaves it unbound) — or is declared `global` or `nonlocal`, where the loop reads
what it builds, where a function that reads the collection may be called while the
loop builds it, where it has an `else`, in a class body, where it adds what a
`yield` evaluates to or assigns its variable or its iterable with a walrus, which
python does not allow in a comprehension, where it has a comment the comprehension
would not keep, where it calls `super()` without arguments before python 3.12, and
for a dict whose key and value could both have effects, which a comprehension
evaluates in the other order

putting a list on separate lines, or on one, rewrites the innermost list with
brackets of its own around the caret — a call's arguments, a definition's
parameters, a display's elements — and only the text between its elements. an
element is a node of the parse, so a lambda among the arguments keeps its commas.
a split list ends with a comma, which tells a formatter it is meant to stay split,
and a joined one loses it unless it is a tuple of one. a list with a comment in it
is refused, since joining would put what follows the comment inside it

### moving a definition

a module's top-level function, class or variable moves to another module of the
project with `by/moveSymbol`, which a code action could not be: the user picks the
target. the definition is cut with its decorators and the comments directly above
it and appended to the target, and what it reads from its old module comes with
it — an import it uses is written in the target too, and dropped from the old module
when nothing else there uses it and no other module imports it from there, and another of the old module's names is imported
from there:

```py
# shapes/util.py                       # shapes/geometry.py, after moving `area`
import math                            import math

RATE = 2                               from shapes.util import RATE

def area(r):                           def area(r):
    return math.pi * r * r * RATE          return math.pi * r * r * RATE
```

every reference in the project follows it: `from shapes.util import area` imports
it from `shapes.geometry` (or, among other names, loses it to a new import),
`shapes.util.area` reads `shapes.geometry.area` with `import shapes.geometry` added,
a module that reached it through `from shapes.util import *` imports it by name, an
`__all__` that listed it moves it, and the old module imports it back when it still
uses it. each import added goes where the project's isort configuration sorts it

the move is refused, with the reason, when the program would not run as before:

- the definition reads a name private to its module, `_SECRET`, which nothing
    outside the module is meant to import — make it public, or move it too
- the two modules would import each other, which python cannot load: the old module
    still uses the definition and the definition reads the old module's names, or the
    target imports the old module already
- the target binds the name already, or binds a name the definition reads to
    something else
- the name is bound more than once in its module
- the definition assigns a name of its module through `global`, which would name
    the target's once moved
- the definition reads a name its module binds more than once, which an import
    would see only as it was when imported
- the definition reads an attribute of its module object, `__name__` or
    `__file__`, which would be the target's
- the definition reads a builtin the target binds to something else
- the modules are not the same kind of file: basedpython does not move into python,
    nor python into basedpython, which reads it differently

`by/prepareMoveSymbol` takes `textDocument` and `position` — in a top-level
definition's name, in a reference to one, or anywhere else in a top-level
definition, its body included, which is then the one moved — and answers `{name, kind, module, uri, range}`: the name, `function`, `class` or `variable`, the dotted name of the module
it is defined in, and its name's range in the file `uri`, which need not be the
document asked about; `null` where there is nothing to move. `by/moveSymbol` takes
the same and `target`, the URI of an existing `.py` or `.by` file of the project
(an editor creates a new module before asking), and answers `{edit, warnings}`, a
`WorkspaceEdit` of `changes` to every file the move changes and the references it
left as they were, each with its file and line, or `{refused, warnings}`, the reason
nothing is moved. two edits at one position of a file apply in the order listed

### pulling members up and pushing them down

a class's members — a method, with the accessors or overloads that share its name,
or a class attribute — move into one of the project's classes it inherits from
with `by/pullMembersUp`, or into the project's classes that name it as a base with
`by/pushMembersDown`, a copy into each that does not define the member itself. each
moves with its decorators and the comments above it, reindented for the body it
lands in: attributes before the methods, methods after them, in the order they
were written

either is refused, with the reason, where the program would no longer mean what it
meant:

- pulled up, a member that reads an attribute of `self` the superclass does not
    have — which the superclass's other subclasses would miss — unless that is pulled
    up with it; that calls `super()` from a class whose one base is not the
    superclass; that a class below the superclass would then find in another class
    than it does, by its method resolution order — another subclass that finds it
    in a mixin or above the superclass, or the class itself in a class it finds
    before the superclass; that reads, where the class body evaluates it — a value,
    a default, a decorator — a name the class body binds that is not pulled with it,
    or a name bound after the superclass is defined; or that the superclass has
    already
- pushed down, a member something reads off the class itself, or off what may be an
    instance of the class rather than of a subclass — another of the class's methods,
    through `self`, included — or of a class nothing in the project subclasses
- a name-mangled `__member`, whose name depends on the class it is written in, a
    method that reads `__class__`, which is the class it is written in, a member that
    reads a name one class finds in a scope around it — a function it is defined in,
    say — and the other finds elsewhere, and, between modules, a member that reads one
    of its module's own names that the other module would have to import from a module
    that imports it, or for any reason a moved
    function is refused for between modules ([moving a definition](#moving-a-definition)):
    a `global` write, a name its module binds more than once, a private name, an
    attribute of its module object, a builtin or a name the other module binds to
    something else. the imports a member reads go with it

`override` follows the move. a method pulled up where nothing above has its name
overrides nothing there, so it loses the modifier, and the methods of that name
below the superclass now override it and gain one — `override def` in basedpython,
`@override` with its import in python. a method pushed down keeps it only where
something above still has the name, and a subclass's own method of that name loses
it once nothing above has the name

`by/prepareMoveMembers` takes `textDocument` and `position` (anywhere in a class)
and answers `{class, members, superclasses, subclasses}`: the class and the classes
of the project above it, nearest first, and below it, each `{name, qualifiedName, uri, range}` with `range` its name; and its members, each `{name, kind, range}`,
`kind` one of `method`, `classmethod`, `staticmethod`, `property` and `attribute`;
`null` outside a class. `by/pullMembersUp` takes the same position, `members`, the
names to move, and `target`, a superclass's `qualifiedName`; `by/pushMembersDown`
takes the position and `members`. both answer as `by/moveSymbol` does,
`{edit, warnings}` or `{refused, warnings}`

### extracting a superclass

`by/extractSuperclass` makes a new class, directly above a class in its module,
that takes the class's bases and the members named, and makes the new class the
class's one base:

```py
class Square(Shape):
    SIDES = 4

    def area(self): ...
```

with `SIDES` extracted into `Polygon`, becomes

```py
class Polygon(Shape):
    SIDES = 4


class Square(Polygon):
    def area(self): ...
```

the new class sits right after the class in its method resolution order, so a moved
member is found where it was found and `super()` in a moved method starts where it
started. it is refused, with the reason, for a member that reads an attribute of
`self` the new class would not have (one the class's own methods assign, or a member
left behind), a name-mangled member, a method that reads `__class__`, which would
be the new class, an attribute of a decorated class (a data
class's field, say), a generic class, a class whose base python treats specially —
`Protocol`, `NamedTuple`, `TypedDict`, `Generic`, an enum — and a name the module
binds already. it takes `textDocument`, `position` (anywhere in the class),
`members`, the names to move, as `by/prepareMoveMembers` lists them, and `name`,
and answers as `by/moveSymbol` does

### changing a signature

`by/changeSignature` gives a function a new parameter list — parameters added,
removed, reordered, renamed, given or stripped of defaults, moved between kinds —
and rewrites every call of it in the project to pass them. each call is bound the
way python binds it, its positional arguments to the positional parameters and its
keywords by name, and written again for the new list: by position while the
parameters still line up, by keyword after that, a keyword staying a keyword, and
keywords in the order the call wrote them. a new parameter is passed the `value`
given, or left to its default. a renamed parameter is renamed where the body reads
it and where calls pass it by keyword, and a method's calls through an instance and
through its class keep their receiver. calls through a name an import gives the
function, `from m import area as a`, are rewritten too. where the new order would
evaluate an argument before one written before it and either could change what
the other does, the call passes them by keyword in the order it wrote them

```py
def area(width, height=1, *, scale=1.0): ...      # (height, width, *, scale=1.0)

area(2, 3)                                        # area(3, 2)
area(2, height=3, scale=2.0)                      # area(width=2, height=3, scale=2.0)
```

it is refused, with the file and line, where a call cannot be rewritten to mean what
it meant: a call that unpacks `*args` or `**kwargs` into the function, unless every
existing call stays valid as it is (a parameter added with a default after the others
leaves them all alone; one added before `**kwargs` does not, since it could take a
keyword); an argument the change would drop when evaluating it could have an
effect; an argument whose order of evaluation cannot be kept because it is only
taken by position; a keyword a call passes into `**kwargs` that a parameter of the
new list would take; a call with nothing to pass for a
parameter without a default; the function used other than called, unless every call
stays valid. and where the result would not be python, or not the same program: a
removed parameter the body still reads, a parameter without a default after one with
a default, two of one name, kinds out of order, or a method another class of the
project overrides or that overrides another's, whose signatures would part

`by/prepareChangeSignature` takes `textDocument` and `position` — in a function's
name or parameters, or in a name that refers to it — and answers `{name, uri, range, parameters}`, its parameters each `{name, kind, default, annotation}` with `kind`
one of `positionalOnly`, `positionalOrKeyword`, `variadic` (`*args`), `keywordOnly`
and `variadicKeyword` (`**kwargs`), and `default` and `annotation` as the source
writes them; a method's `self` or `cls` is not listed and stays as it is.
`by/changeSignature` takes the same position and `parameters`, the whole new list
in order, each `{name, oldName?, kind, default?, value?}`: `oldName` the parameter it
was, absent for a new one; `default` written as source, absent for none; `value` what
each call passes a new one. it answers as `by/moveSymbol` does, `{edit, warnings}` or
`{refused, warnings}`

## unwrap and remove

`by/unwrap` answers what an editor's *unwrap/remove* offers around a range, innermost
first: each `if`, loop, `try` or `with` around it can be replaced by one of its
suites, dedented to where the statement stood, and each `elif`, `else`, `except` or
`finally` the range is in can be removed. a `try` unwraps into the path that raises
nothing — its suite, then its `else`, then its `finally`

these change what a program does, which is what they are asked for, so they are
not refused for that. they are refused when the result would not say what it seems
to: a loop whose body `break`s or `continue`s does not unwrap, since the jump would
leave some other loop or none, a `try` keeps an `except` or a `finally`, and a
`try` with a `finally` does not unwrap when its suite can `return`, `raise`,
`break` or `continue`, since the `finally` would no longer run after it. comments
after a kept suite's last statement go with the suite

each option comes with the range it rewrites and the ranges of it that stay, for an
editor to mark the one selected, and its edits are many small ones — the header
deleted, each line of a kept suite dedented — so the caret stays where it was in
the text. the lines of a string literal that spans several lines are left as they
are, because their indentation is part of the string

## semantic tokens

the legend `initialize` answers with, in its order — a token's type and
modifiers are sent as positions in these lists. the names LSP defines are used
where one fits; the others are basedpython's own

| token type        | what it is                                                                                                      |
| ----------------- | --------------------------------------------------------------------------------------------------------------- |
| `namespace`       | a module                                                                                                        |
| `class`           | a class, or a type alias                                                                                        |
| `parameter`       | a parameter, and the name a keyword argument is passed by                                                       |
| `selfParameter`   | a method's `self`                                                                                               |
| `clsParameter`    | a class method's `cls`                                                                                          |
| `variable`        | a variable or an attribute                                                                                      |
| `property`        | a property                                                                                                      |
| `function`        | a function                                                                                                      |
| `method`          | a method                                                                                                        |
| `keyword`         | a keyword spelled like a name (`var`, `let`, `typeof`), and a directive of a format spec or a `strftime` format |
| `string`          | a string literal                                                                                                |
| `number`          | a number literal                                                                                                |
| `decorator`       | a decorator's name                                                                                              |
| `builtinConstant` | `None`, `True`, `False`                                                                                         |
| `typeParameter`   | a type parameter                                                                                                |
| `comment`         | a comment in a django template                                                                                  |
| `operator`        | punctuation in a django template or in a script's `# /// script` metadata                                       |
| `regexp`          | a date or time in a script's `# /// script` metadata                                                            |

| modifier          | the token is                                                                       |
| ----------------- | ---------------------------------------------------------------------------------- |
| `definition`      | where the name is defined, rather than used                                        |
| `readonly`        | a name written as a constant, `MAX_SIZE`, or a property with no setter             |
| `async`           | the name of an `async def` where it is defined                                     |
| `documentation`   | a docstring (on a `string`)                                                        |
| `defaultLibrary`  | a name the `builtins` module defines — `print`, `len`, `str` — or a django builtin |
| `functionScope`   | a name bound in the scope of a function, a lambda or a comprehension               |
| `annotation`      | part of a type expression                                                          |
| `dunder`          | a dunder name, `__init__` or `__name__`                                            |
| `keywordArgument` | the name a keyword argument is passed by, `scale` in `total(scale=2.0)`            |

`functionScope` marks a parameter or a local, as against a module global or a
class attribute. it is the binding that decides, not where the name is written:
a global read inside a function is not marked, a `nonlocal` name is, and an
assignment expression in a comprehension at the top of a module binds a global.
a `def` or a class nested in a function is a local too, and its name is marked
where it is declared as well as where it is used. an editor that colours each
local by its name ("rainbow" highlighting) reads it off this modifier

a name is a `method` where the `def` it names is made in a class body, wherever
the name is used: `field` called in a dataclass's body is a `function`, the one
`dataclasses` defines

`annotation` marks every token of a type expression: a parameter's or a
variable's annotation, what a function returns or raises, a type alias's value,
a type parameter's bound or default, and an argument the checker reads as a type,
as `cast(list[int], x)` does its first. a default value is not one. `dunder`
marks a dunder wherever it is defined or read; a name that only starts with
underscores is not one

## keyword highlighting

`textDocument/documentHighlight` on a keyword answers the keywords that belong
with it rather than the occurrences of a symbol: the `elif`s and `else` of an
`if`, the `except`, `else` and `finally` of a `try`, the `return`s of a `def`.
the pairing is read off the parse tree, so `match = 1` is an assignment and
nothing lights up on it. a position that is not a keyword is answered the way it
always was

## typing a clause

a clause that continues a compound statement — `elif`, `else`, `except`,
`finally`, a further `case` — belongs at the indentation of the clause it
continues, and an editor starts a new line at the indentation of the body it
closes. the server advertises `textDocument/onTypeFormatting` with `:` as its
trigger, and answers the `:` that finishes such a header with the edit that
moves the clause out:

```by
for item in items:
    if item:
        handle(item)
        else:   # typed here, moved out to the `if` when the `:` goes in
```

the statement it moves to is the innermost one still open above the line that
can take that clause next and is not indented deeper than the line already is.
a clause is only ever moved out, so an `else:` typed at the loop's column stays
with the loop, and an `if` that already has its `else` is passed over. which
statements are open is read from a parse of the text above the line alone,
since the clause itself is a syntax error until it is where it belongs. a `:`
that does not end the header — in a slice, a dict or a lambda — and
`case: int = 1`, an annotated variable called `case`, are answered with no edit

## completing a statement

`by/completeStatement` finishes the statement at a position and starts the line
after it, which is what an editor's *complete current statement* does:

```json
{
  "textDocument": { "uri": "file:///src/main.by" },
  "position": { "line": 1, "character": 10 },
  "options": { "tabSize": 4, "insertSpaces": true },
  "textHash": "…"
}
```

a single-quoted string left open on the line is closed, the brackets left open
on it are closed innermost first, a clause header gets its `:` — and a `def`
its `()` — and a clause that continues a compound statement is moved out the
way typing its `:` moves it. a statement carried on to lines below that parse,
such as a call spread over several lines, is completed where it ends rather
than on the line asked about; a bracket the following lines do not close is
closed where it was left open. the new line goes inside the suite a header
opens, at the indentation of the suite's statements when it has some already
and one level deeper than the header otherwise, and after any other statement
at the statement's own indentation. `options` says what one level is, the way
`textDocument/onTypeFormatting` does: `tabSize` spaces, or a tab

the reply is the edits, against the document as it was asked about, and where
the caret goes in the document as the edits leave it:

```json
{
  "edits": [
    {
      "range": {
        "start": { "line": 1, "character": 10 },
        "end": { "line": 1, "character": 10 }
      },
      "newText": "):\n        "
    }
  ],
  "caret": { "line": 2, "character": 8 }
}
```

like the other document requests it takes `textHash`, and it is `null` when
language services are off

## writing a docstring

typing `"""` and Enter at the top of a function's body leaves an empty docstring,
and an editor asks `by/docstringStub` with a position inside it for the stub to
fill it with:

```json
{
  "textDocument": { "uri": "file:///src/shapes.by" },
  "position": { "line": 2, "character": 11 },
  "format": "google"
}
```

the answer replaces the whole literal, quotes and prefix kept, and says where the
caret goes in the document once `newText` is in it — the summary line, which is
written indented and empty:

```json
{
  "range": { "start": { "line": 2, "character": 8 }, "end": { "line": 2, "character": 14 } },
  "newText": "\"\"\"\n        \n\n        Args:\n            item:\n\n        Returns:\n        \"\"\"",
  "caret": { "line": 3, "character": 8 }
}
```

the stub lists every parameter but a method's receiver — the instance or class
a method is passed, and the class `__new__` is — with `*args` and `**kwargs`, and
says what the function hands back: `Yields` for a generator, `Returns` when its
annotation is something other than `None`, `NoReturn` or `Never`, or with no
annotation when it returns a value. `format` is `rest` (`:param item:`),
`google`, `numpy` or `plain` (the summary alone); without one, the format is the
one the file's other docstrings are written in, and reStructuredText, as PyCharm
writes by default, when none of them says. the answer is `null` anywhere that is
not inside a triple-quoted docstring of a function holding nothing but whitespace
on a line of its own

## rules

each diagnostic names the rule it is reported under as its `code`,
`unresolved-reference`. an editor that gives every rule an entry of its own, an
inspection whose severity its user can change, asks `by/rules`, with no
parameters, for the rules before any diagnostic arrives. the answer is every
rule, by name, each with its one-line `summary`, its `defaultLevel` (`ignore`,
`warn` or `error`), the level it has when the configuration names no other, and
its `status` (`stable`, `preview` or `deprecated`):

```json
[
  { "name": "unresolved-reference", "summary": "detects references to names that are not defined", "defaultLevel": "error", "status": "stable" }
]
```

a removed rule is left out. `by/explainRule` gives a rule's whole
documentation, and the linter's rules, `F401` among them, are `buff`'s. rules
have no categories

## suppressing a diagnostic

every diagnostic has two quick fixes that silence it rather than fix it.
*ignore for this line* writes `# ty: ignore[<rule>]` at the end of its line,
and *ignore in this file* writes it on a line of its own at the top of the
file, before any code, which suppresses the rule throughout the file. a
suppression already there takes the rule instead of a second comment being
written, and a `#!` line stays first. ty has no suppression that covers one
function or one class, so neither does the editor

## hover

a hover opens with what the name is: its kind and the dotted name it is
reached by from outside its module, `method shapes.base.Circle.area`. the kinds
are `module`, `class`, `function`, `method`, `property`, `attribute` (a name a
class body binds, or one a method assigns on `self`), `variable`, `parameter`,
`type parameter` and `type alias`. a name local to a function has only its own
name, and so does a parameter or a type parameter

a decorated `def` is shown as its source writes it, its decorators over its own
signature and with its own docstring:

```by
@functools.cache
def discount(self, x: int) -> int
```

and the docstring is the def's, not that of the type its decorators bind the name
to. when that type is not the def's own function, which for a decorator returning
a wrapper of its own is `def wrapper(*args, **kwargs)`, it is shown first, before
the def as written. that holds wherever the name is hovered, a call of it included,
except a use of a property, which reads the property's value

## what a name declares

an editor's *copy reference* copies a declaration's dotted name, and a search
for usages names what it searched for. both ask `by/declaration`, with the
position of a name where it is declared or where it is used:

```json
{
  "textDocument": { "uri": "file:///src/main.by" },
  "position": { "line": 3, "character": 16 }
}
```

and the answer is what the name declares, or refers to, and where:

```json
{
  "kind": "method",
  "name": "area",
  "qualifiedName": "shapes.base.Circle.area",
  "originSelectionRange": {
    "start": { "line": 3, "character": 14 },
    "end": { "line": 3, "character": 18 }
  },
  "uri": "file:///src/shapes/base.by",
  "range": {
    "start": { "line": 4, "character": 4 },
    "end": { "line": 5, "character": 26 }
  },
  "selectionRange": {
    "start": { "line": 4, "character": 8 },
    "end": { "line": 4, "character": 12 }
  }
}
```

`kind` is one of the kinds a hover names. `qualifiedName` is the module's
dotted name, each class the declaration is made in, and the name; an attribute
a method assigns on `self` is the class's, `shapes.base.Circle.radius`. it is
`null` for a name local to a function, a parameter, a type parameter, and a
declaration in a file no module is imported from. `originSelectionRange` is the
name at the position, `range` the whole declaration and `selectionRange` its
name. a called class is the class, not the `__init__` the call runs, and a
decorated `def` is the `def`. the answer is `null` where there is no name, and
for a name whose definitions declare different things

## what each usage does

an editor's *find usages* groups the usages of a name by what each does with
it, as PyCharm's Python groups them: the imports of a class apart from its uses
as a base and in annotations. a `Location` has nowhere to say that, so
`by/references` takes what `textDocument/references` takes, a position and
`context.includeDeclaration`, and answers the same references in the same
order, each with its `kind`:

```json
[
  { "uri": "file:///src/shapes/util.by", "range": { "start": { "line": 0, "character": 31 }, "end": { "line": 0, "character": 36 } }, "kind": "import" },
  { "uri": "file:///src/shapes/util.by", "range": { "start": { "line": 3, "character": 25 }, "end": { "line": 3, "character": 30 } }, "kind": "typeHint" }
]
```

| kind          | the reference is                                                                                                                                                                               |
| ------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `import`      | in an `import` or `from ... import` statement                                                                                                                                                  |
| `isinstance`  | among the classes an `isinstance` call tests against: its second argument, or in a tuple there                                                                                                 |
| `typeHint`    | in a type expression: an annotation, a return or `raises` annotation, a type alias's value, a type parameter's bound or default, or an argument a call takes as a type, such as `cast`'s first |
| `superclass`  | in the parentheses of a class's header, a base or a keyword such as `metaclass=`                                                                                                               |
| `write`       | binding the name: an assignment, an augmented assignment, a `del`, a `for` or `with` target                                                                                                    |
| `read`        | reading it, which calling it is                                                                                                                                                                |
| `declaration` | the name where it is declared, there with `includeDeclaration`                                                                                                                                 |

a reference is the first of these that holds, from the top, so an import is an
`import` although it binds the name, and of nested groups the nearest wins: a
class in `cast`'s first argument in a base list is a `typeHint`. the answer is
`null` where `textDocument/references` answers `null`

## type info

an editor's *type info* shows the type of an expression, and which expression
is meant depends on more than the name under the cursor: on `text` in
`len(text) + 1`, a reader may be asking about `text`, about `len(text)` or about
the sum. the editor asks `by/expressionTypes` with the selection, or an empty
range at the caret:

```json
{
  "textDocument": { "uri": "file:///src/main.by" },
  "range": {
    "start": { "line": 1, "character": 17 },
    "end": { "line": 1, "character": 17 }
  }
}
```

and the answer is every expression of the statement that contains the whole
range, innermost first, each with its `range` and its `type` as the document's
language spells it:

```json
[
  { "range": { "start": { "line": 1, "character": 15 }, "end": { "line": 1, "character": 19 } }, "type": "str" },
  { "range": { "start": { "line": 1, "character": 11 }, "end": { "line": 1, "character": 20 } }, "type": "int" },
  { "range": { "start": { "line": 1, "character": 11 }, "end": { "line": 1, "character": 24 } }, "type": "int" }
]
```

with a selection the first is the smallest expression containing it, which is
the selected one when the selection is exactly an expression. the targets a
value is unpacked into, `kind, _, size` in `kind, _, size = spec.partition(":")`,
are the tuple of what each is bound to, `(str, str, str)`, a starred one standing
for any number of its list's elements. an expression ty has no type for, such as
one inside a string annotation, is left out. the answer
is an empty list where the range is in no expression, a `def`'s name or a
keyword, and `null` when language services are off

## extend selection

`textDocument/selectionRange` grows a selection the way a reader reads the
code, not only node by node. on `"a"` in `xs = ["a", "b"]` the ranges are the
string's text, the string, `"a",` with its comma and the space after it, `"a", "b"` without the
brackets, the list, the assignment, and then the assignment's whole line. a
last element takes the comma before it, `, "b"`. a statement is offered as the
whole lines it is written on, indentation and newline included, when it shares
them with nothing but a comment, and the statements of a body are offered
together without their header, so a selection inside a method grows to the
method's body, then to the method with its decorators as whole lines, then to
the class

## go to declaration

`textDocument/definition` and `textDocument/declaration` on the name of a
called class go to the constructor the call runs: on `Circle` in `Circle(3)`,
the `__init__` (or `__new__`) that `Circle` writes, or that the base it
inherits one from writes. a call of a class is a call of its constructor, and
the constructor is where its parameters are. a class that writes none, and so
runs `object`'s, goes to the class. the class itself is always a
`textDocument/typeDefinition` away, and a rename or a search for references
from the same name is about the class

## go to super

`textDocument/implementation` on a method goes down, to the methods that
override it. a decorated override is its own `def`, not the function its
decorator returns, which a decorator like `logged`, returning a `wrapper` of its
own, defines somewhere else. `by/superMembers` goes up: given a position on the name a class
member is declared with — a `def` or a nested class, a name the class body
assigns, annotates or captures, or an import's alias — it answers the
superclass members that member overrides, each with its class, its file and the
name to land on. a class goes up with `by/mro`

`by/mro` takes a position on a class's name, where it is declared or used, as
`textDocument/prepareTypeHierarchy` does, and answers every class it inherits
from in python's lookup order, its method resolution order without itself:
`Shape`, `ABC`, `object` for `class Circle(Shape)` with `class Shape(ABC)`, and
a diamond's shared base after both its sides. each is a type hierarchy item as
`typeHierarchy/supertypes` answers one, which gives only the direct bases. the
answer is `null` where there is no class

what counts as an override is the override checks' own walk up the MRO, from
the class as it is written rather than as a decorator returns it. of what that
walk finds, a member goes to the nearest declaration along each branch, in MRO
order: a method overriding `B.f`, where `B.f` overrides `A.f`, goes to `B.f`,
and a class with two bases that each declare the method goes to both. the first
of them is the one `missing-override-decorator` names. a `private` member
overrides nothing, since it is emitted under a name of its own, and a member a
superclass synthesizes rather than writes — a dataclass's `__init__` — goes to
that superclass, marked `synthesized`. each is marked `abstract` when it is
abstract where it is declared — an `@abstractmethod`, or a protocol method with
no implementation — so that a member overriding it implements it. the answer is
`null` when no class member is declared at the position, and an empty list for
one that overrides nothing

an answer depends on more than the text of the document it is about: on every
file the document's classes inherit from, and the server announces no change to
it. a client that keeps one asks again after it opens, edits or closes any
document of the workspace, not only that one, and on
`workspace/inlayHint/refresh`, which the server sends, to a client that supports
it, after a change on disk or to the environment changes what it answers from

`by/documentSuperMembers` answers the same for a whole document in one request,
from the same code: every class member that overrides something, with its
class, the range of the name it is declared with, whether it is `abstract`
itself, and its `superMembers` as `by/superMembers` gives them. it is what an
editor asks to draw an "overrides" marker beside each member, on every pass over
the document. a property with `get` and `set` blocks is one member, listed once.
constructors and the methods they call — `__init__`, `__new__`,
`__post_init__`, `__init_subclass__` — are left out, since neither
`invalid-method-override` nor `missing-override-decorator` holds one to what it
overrides, and `by/superMembers` still answers one. like the other document
requests it takes `textHash`, and answers a document the client has not opened
from the file on disk

## usage counts

`by/documentUsageCounts` answers, for a whole document, how many places use
each of its declarations and how many classes subclass it or members override
it: what an editor draws above each one as "3 usages" or "2 overrides". the
declarations are the document's outline at the depth a reader scans a module
at — its classes and functions, and the methods and properties of those
classes. a dunder method such as `__eq__` is there for what overrides it, but
its usages are not counted, the language calling it rather than its name: it
has no `references`, even when they are asked for. a constructor and the
methods it calls, `__init__`, `__new__`, `__post_init__` and
`__init_subclass__`, are left out, as `by/documentSuperMembers` leaves them
out: a subclass's constructor takes what its own class needs, so it overrides
nothing and nothing overrides it. a module's dunder function is left out, and so
is an `extension` block, which names a class it does not declare

each declaration comes with its `name`, its `kind` (`class`, `function` or
`method`), its whole `range` from its first decorator, its `selectionRange` on
its name, and the counts named in the request's `counts`:

| count             | what it is                                                                                  |
| ----------------- | ------------------------------------------------------------------------------------------- |
| `references`      | the places `textDocument/references` finds from the name, the declaration not among them    |
| `implementations` | of a class or a method, what `textDocument/implementation` finds from the name, less itself |

so the count is the number of places the click on it goes to, when a client
sends that request from the name. a method with implementations is marked
`abstract` when it is an `@abstractmethod` or a protocol member with no body,
so that what overrides it implements it. a count not named is not worked out

with `implementations` comes `implementors`, the places it counts, in the
order `textDocument/implementation` answers them, so that an editor can say
"is overridden in Circle, Square" without asking. each is a subclass of the
class, or a member overriding the method, with the `uri`, `range` and
`selectionRange` it is declared at and the class it is in: `className`, the
subclass itself or the class whose member overrides, and `qualifiedName`, that
class's dotted name, `shapes.base.Circle`, absent for a class local to a
function

```json
{
  "name": "area", "kind": "method", "implementations": 1, "abstract": true,
  "implementors": [
    {
      "className": "Circle", "qualifiedName": "shapes.base.Circle",
      "uri": "file:///src/shapes/base.by",
      "range": { "start": { "line": 32, "character": 4 }, "end": { "line": 34, "character": 41 } },
      "selectionRange": { "start": { "line": 33, "character": 8 }, "end": { "line": 33, "character": 12 } }
    }
  ]
}
```

each count is a search of the workspace, made for all the declarations of the
document in one pass: each file is read once, and searched for each declaration
whose name it holds. the searches run one at a time, on a thread of their own,
so however many a client has asked for, the requests someone is waiting on,
such as a hover or a completion, never queue behind them. a count the client
cancels stops where it is, its work in parallel included, so a client that asks
again on every edit and cancels the ask the edit made stale leaves no searches
running behind it

a count depends on every file of the workspace, and the server announces no
change to it: a client asks again after any document of the workspace changes,
and asks for every count it draws in one request. it is not a
`textDocument/codeLens`, since a client that draws lenses asks for them again
only when their own document changes. like the other document requests it takes
`textHash`

## tests and what they test

`by/testSubjects` and `by/subjectTests` answer an editor's *go to test subject*
and *go to test*. a test exercises what it calls, as call hierarchy sees a call:
a function or a method called by name or as an attribute, or a class
constructed. which declarations are tests is pytest's collection, as
`by/testItems` reports it. nothing goes by file names: `test_circle.by` tests
whatever it calls, wherever that is, and the tests of a class are found whatever
their files are called

`by/testSubjects` takes a position in a test, in a class tests are collected
under, or anywhere else in a module tests are collected from, standing for that
test, every test under the class, or every test in the module. it answers the
project's own functions, methods and classes those tests call, each once, in the
order they are first called: not what is declared in a test module or a
`conftest`, and nothing outside the project. each item has its `name` within its
module with the classes it is declared in (`Circle.area`), its `kind`, its
`module`, and its `uri`, `range` and `selectionRange`. the answer is `null` for
a document no test is collected from

`by/subjectTests` takes a position in a function, a method or a class, or
anywhere else in a module, which stands for every function and class it
declares; a class stands for itself and its methods. it answers the collected
tests that call it, as items named by their pytest node id after the file
(`TestCircle::test_radius`), by file and then in source order, and `newTest`,
where a test of it would go: `test_` and the last part of its module's name, in
the directory most of the project's test modules are in, or in `tests` under the
project root when there are none. a test module of that name there already is
the one, whether it is `.by` or `.py`; a new one is written in what the subject's
file is. `newTest` says whether that module `exists`, the `module` to import
from, the name to `import` (absent for a module, which a test imports whole), a
`testName` the module does not define already, and for a module that exists the
`importEdits` that import the name into it, none when it does already. `newTest`
is `null` for a test module, or a module with no name to import it by. like the
other document requests both take `textHash`, and with it answer a document the
client has not opened from the file on disk

`by/moduleName` takes the `uri` of a file or a directory and answers the dotted
name it is imported by, from the search path it is under: a src-layout
project's `src/shapes/circle.by` is `shapes.circle`, and `src/shapes` and
`src/shapes/__init__.by` are both `shapes`. it is `null` for a path no module is
imported from, and for a file that resolution would never reach

### running tests in an IDE

`by run pytest` reports to an IDE's test runner when `BY_TEST_REPORTER=teamcity`
is set: the pytest plugin `by run` puts in the tree writes the run to standard
output as TeamCity service messages, alongside pytest's own output, which is left
as the arguments ask for. what it writes:

- `enteredTheMatrix` as the session starts, so a client knows the plugin is
    reporting before the first test, and `testCount` with how many tests were
    collected
- a file that failed to collect, a test module whose import raised, as a test of
    its own at the top level, named by its file: `testStarted`, `testFailed` with the
    error's line as `message` and the whole report as `details`, and `testFinished`;
    a file skipped as a whole has `testIgnored` instead
- for each test, the suites it is in: its file, named by the project file it was
    transpiled from or copied out of relative to the project, `tests/test_calc.by`,
    and each class it is collected under, `TestGroup`, each opened before the first
    of its tests and closed after the last
- `testStarted` with the test's `name` as pytest gives it, `test_sum` or
    `test_add[1-2]`, its node id as `metainfo`, `tests/test_calc.py::test_sum`, which
    is what pytest is given to run it again, and `captureStandardOutput` `false`
    while pytest captures its output, which follows as `testStdOut` and
    `testStdErr`
- `testFailed` when any of its setup, call or teardown failed: the error's line as
    `message`, prefixed `error at setup:` or `error at teardown:` for those, and
    pytest's report of the failure as `details`, every frame at its `.by` line. when
    the failure is an assertion that failed comparing two values with `==`, and that
    comparison is the one it made, it carries `type='comparisonFailure'`, `actual`,
    the left side, and `expected`, the right side, each a string as it is and
    anything else as `pprint` writes it. `assert a == b or c == d` carries neither
- `testIgnored` for a test skipped, with the reason, or failing as expected, with
    `expected failure:` and the reason, then `testFinished` with its `duration` in
    milliseconds

every `locationHint` is `file://` and the absolute path of the project file, with
`:` and the 1-based line of the `def` or `class` for a test or a class. any other
value of `BY_TEST_REPORTER` is refused as a usage error

## endpoints

`by/endpoints` answers an editor's endpoints view: every HTTP route the project
serves, from FastAPI and from django. it takes `{}` and answers a list, from every
project in the session, ordered by path and then by method, each

```json
{
  "framework": "fastapi",
  "methods": ["GET"],
  "path": "/v1/items/{item_id}",
  "handler": {
    "name": "app.items.read_item",
    "uri": "file:///project/app/items.by",
    "range": {"start": {"line": 6, "character": 0}, "end": {"line": 8, "character": 22}},
    "selectionRange": {"start": {"line": 7, "character": 4}, "end": {"line": 7, "character": 13}}
  },
  "declaration": {
    "uri": "file:///project/app/items.by",
    "range": {"start": {"line": 6, "character": 1}, "end": {"line": 6, "character": 25}}
  },
  "application": {
    "name": "app.main.app",
    "uri": "file:///project/app/main.by",
    "range": {"start": {"line": 4, "character": 0}, "end": {"line": 4, "character": 15}},
    "selectionRange": {"start": {"line": 4, "character": 0}, "end": {"line": 4, "character": 3}}
  }
}
```

`framework` is `fastapi` or `django`. `methods` are upper case, and empty for a
route that serves every method, as each of django's does. `path` is the whole
path, every prefix the route is mounted under included, with its parameters as
the route writes them, `{item_id}` or `<int:pk>`; a `re_path()` pattern is
joined to its prefixes with each part's `^` and `$` anchors dropped. `handler` is
the function, or for a django view built by `as_view()` the class, with its
qualified name, its whole declaration as `range` and its name as
`selectionRange`; `declaration` is where the route is declared, the decorator
without its `@`, the `add_api_route(...)` or the `path(...)`; `application` is the
FastAPI application that serves it, or the urlconf module django starts at, and
is absent for a route on a router no application includes. a route served under
two applications, or mounted twice, is listed once for each path.

every part is read through what the code means. a FastAPI route is a decorator,
or an `add_api_route` call, that resolves to a method of `fastapi.FastAPI` or
`fastapi.APIRouter`, `get` to `trace` for that method and `api_route` for its
`methods=` (GET without it), however the application or router is named or
imported; a router's routes are mounted by an `include_router` handed the same
router, under the include's `prefix` and the router's own `APIRouter(prefix=)`,
through any number of routers between it and an application. a django route is
an entry of a urlconf module's `urlpatterns` that calls `django.urls.path` or
`re_path`, read from the module `ROOT_URLCONF` names, or, in a project whose
settings name none, from every module with `urlpatterns` that no other includes;
an `include()` is followed to the module a string names, a module object, a list
of patterns, or the patterns of a `(patterns, app_name)` pair. a path, a prefix
or a method that is not a string ty knows exactly — a literal, or a constant
holding one — leaves its route out rather than listing it at a path it may not
have, and so does a FastAPI router built somewhere other than where it is
assigned, and a route whose handler, declaration or application cannot be opened

## merges and diffs

an editor's merge and diff tools compare versions of a file no document holds, so
both requests take texts, and an optional `uri` naming the file they are versions
of: its extension says how they are parsed (python without one), and the isort
configuration buff reads for it sorts imports (the first project's root's
without one).

`by/resolveMergeConflicts` takes the merge's three versions, `base`, `ours` and
`theirs`, each a whole file, and `conflicts`, the lines each conflict spans in
each version, `{"base": {"start": 3, "end": 3}, "ours": {"start": 3, "end": 5}, "theirs": {"start": 3, "end": 4}}`, 0-based with `end` excluded. it answers, for
each conflict in order, the text that replaces its lines, or `null` where it
cannot be resolved. a conflict is read as what its lines hold in each version's
whole syntax tree, and is resolved only where the lines are in the same body or
display in all three versions, and the two sides' changes provably touch nothing
in common:

- import statements: every import either side added, without those either side
    removed, written as buff's isort configuration sorts and writes them. refused
    when both sides changed what one name is bound to differently, when the result
    binds one name to two things (`from a import x` and `from b import x`), when a
    side imports a name the other side binds elsewhere other than by an import of
    the same thing, when either side has a star import among them, and when a
    comment is among them
- functions and classes, methods among them, that each side added: each kept
    where it was added, at least two blank lines apart at the top level and one
    elsewhere. refused when a side changed or removed a statement or added
    anything else, or added a name the other side's version binds anywhere in that
    scope, in any form, or, in a class, assigns as an attribute anywhere in the
    file; a star import in that scope refuses too. what both sides added first or
    last in one place is kept once. definitions both sides added differently in
    one place are kept, ours first, only when running each binds its name and does
    nothing else: no decorator, no base class or class keyword, and every default
    and annotation a name or a literal
- elements of a list, set, tuple or dict display, or of a decorator list, that
    each side added in a different place: each kept where it was added. refused
    when both sides added different elements in one place, since a display
    evaluates and keeps its elements in order and decorators apply in order, so
    `__all__` entries both sides append are left to you, and when a side changed
    or removed one, two elements share a line, an element that would no longer be
    last has no comma after it, or both sides add one key of a dict
- sides whose statements or elements are the same syntax tree with the same
    comments, which differ only in formatting: ours

anything else is `null`: lines that are not whole statements or whole elements,
and two sides that changed one statement, as `LIMIT = 20` and `LIMIT = 30` did.

`by/diffIgnoredRanges` takes `{"text": "…"}` and answers what a diff ignoring
imports and formatting may ignore in it, `{"imports": [...], "formatting": [...]}`,
each a list of ranges. `imports` is each import statement, wherever it is.
`formatting` is the text python's tokenizer discards without its being
significant: whitespace between two tokens of a logical line, the line breaks and
indentation of a line continued inside brackets or after a backslash, and blank
lines. the indentation a logical line starts with is not formatting, since it
says which block the line is in, and nor is anything inside an f-string or a
t-string, where `f"{x = }"` prints the spaces around its `=`. two texts that
differ only in what is ignored tokenize alike. a change that is formatting
across lines, as rewrapping a call is, is ignored only where each side's
whitespace is, which is as much as one text can say: whether two texts differ
only in formatting is a question about both, which `by/resolveMergeConflicts`
answers for a conflict

## override and implement members

an editor's *override methods* and *implement methods* ask twice: once for what
the class can override, to let its user pick, and once for the declarations of
the ones picked. a code action cannot carry that, since its edit is worked out
before anything is picked

`by/overridableMembers` takes a position in a class — its header, its body, or
an indented line below its last statement, where a new member would go — and
answers the class's name and the methods it inherits without defining them, in
MRO order of the superclass each comes from. each comes with that superclass and
its module, whether it is `abstract` where it is inherited from (what
`abstract-instantiation` counts as left unimplemented, so the ones *implement
methods* offers), whether it is a `method`, a `classMethod` or a `staticMethod`,
and its `signature`: the declaration that would be written, from its name on.
`object`'s methods are listed, last; a `final` or `private` method, and the
constructors and the methods they call, are not. the answer is `null` where
there is no class, and for a file that is not basedpython

`by/overrideMembers` takes the same position and the names picked, and answers
the edits that write their declarations, in the order they were listed:

```by
from pathlib import Path

class Square(Shape):
    side = 1.0

    override def area(self) -> float:
        ...

    override def save(self, to: Path):
        super.save(to)
```

each is declared `override`, with `class`, `static`, `async` or `protected` when
the method it overrides is — an async generator is not `async`, and returns the
generator its call makes — and with the signature the class inherits: a generic
superclass's type parameters filled in the way the class fills them, a method's
own type parameter renamed where it would hide one of the class's, an
overloaded method's overloads one declaration each. a type the file does not
reach yet is imported, and one it cannot name at all is left out, since a
parameter left unannotated takes the annotation of the one it overrides. a
default that is a value is left out of a method with a body for the same
reason, and one that is an expression is written as its base wrote it, with
each name in it imported, or written through its module where the file means
something else by it. the body
calls the method through `super`, or a static method, which has no `super`,
through its class's name; a method that is abstract has nothing to call, and
its body is `...`, which the checker reports until it is written. `-> None` is
not written

the declarations go after the statement the position is in or follows, or at the
end of the body for a position in the header, and replace a body that is only
`pass` or `...`. the answer's `selection` is the first declaration's body as a
range in the text the edits leave behind, which is where the user goes next. it
is `null` when none of the names is a method the class could override

## imports

an `unresolved-reference` is offered an import for every module with a member
of that name. when there is exactly one, its quick fix carries
`{"unambiguousImport": true}` in its `data`: nothing is left to choose, so an
editor may add it as the code is typed, the way IDEs add unambiguous imports on
the fly. `isPreferred` does not say this — every import that resolves a name is
preferred, since each properly fixes the diagnostic — and the mark changes
nothing for a client that does not read it

code copied from one module and pasted into another needs the imports it was
written against. `by/copiedImports` takes the copied text and the position in
the document it was copied from, one piece per caret, and answers each name the
text uses that the document's global scope binds there: `name`, the absolute
`module`, and the `member` imported from it, absent when the name is the module.
a name the text binds itself, a local of the scope it came from, and a builtin
are not answered. it takes the text rather than a range because a cut takes the
text out of the document, while the scope it was cut from still binds what its
names meant. `by/pastedImports` takes those imports, the document they were
pasted into and the range the paste now occupies, and answers the ones whose name
is not bound in the scope the paste landed in, with edits that add all of them.
a name bound there to something else is left alone, and so is an import of the
document's own module or of a module it cannot resolve. the edits of some of the
imports are not some of the edits — two imports from one module go into one
statement — so a client that lets its user choose asks again with the ones
chosen. both take `textHash`

## the whole project's diagnostics

with `diagnosticMode` set to `workspace`, the server checks every file in the
project, open or not, and reports what it finds two ways:

| request                | answers                                                                |
| ---------------------- | ---------------------------------------------------------------------- |
| `workspace/diagnostic` | once something differs from the result ids sent — held open until then |
| `by/checkWorkspace`    | as soon as the check is done, whether or not anything differs          |

both take the same parameters and report the same thing, file by file, with an
unchanged report for a file whose result id still matches. `workspace/diagnostic`
is the one an editor keeps a problems list current with: it asks again as soon
as it is answered, and the server answers when an edit or a change on disk
changes something. `by/checkWorkspace` is for a client that asks once — an
inspection over the whole project, a batch run with no editor — and would
otherwise wait forever on a project with nothing wrong in it. it is refused,
rather than answered empty, when the diagnostic mode does not check the
workspace

they differ in how the report arrives. `workspace/diagnostic` streams it as
partial results when the client sends a `partialResultToken`, and reports
progress under the client's `workDoneToken`. `by/checkWorkspace` ignores both
tokens: its report comes whole, in the response, and its progress, when the
client supports `window/workDoneProgress`, comes under a token the server
creates

`by/checkWorkspace` can take a while to answer, for two reasons:

- an edit or a change on disk that lands while the workspace is being checked
    starts the check again, on the changed workspace. the answer describes the
    workspace as it was when a check finished, and a workspace that never stops
    changing is never answered
- while a PEP 723 script's environment is being set up for the first time, the
    workspace is not checked, and the request waits until the environment is
    there. the first setup installs the script's dependencies, so it can take as
    long as that does

## the program model

a run configuration, a test list and a "go to generated file" are all questions
about the project rather than about a position in a file, and each used to be an
editor's own guess at a layout the build decides. the server answers them
instead, from the same reading the transpiler and the checker use:

| request            | answers                                                                     |
| ------------------ | --------------------------------------------------------------------------- |
| `by/entryPoint`    | whether a module has a `main`, and the command line the transpiler gives it |
| `by/testItems`     | the tests pytest collects from a document, with their node ids              |
| `by/runModules`    | the modules `by run` can be given, and the one `run.main` names             |
| `by/buildOutput`   | where a project's build writes, and which output came from which source     |
| `by/syntaxOutline` | a document's suites, clause keywords, declared names, lists and strings     |

`by/buildOutput` maps both ways: given a `.by` it names the file the build writes
it to, given a file in the build directory it names the source. the path inside
the build follows the *module* tree, not the directory tree, so a src-layout
project's `src/pkg/main.by` is `build/pkg/main.py` — which is why this is the
build's answer rather than a path an editor can assemble

an editor that runs the build itself, for a *build project* action, takes the
build's diagnostics from its output rather than reading them as prose:
`by build --output-format github`, or `TY_OUTPUT_FORMAT=github` in the build's
environment as `by check` reads it, prints each diagnostic on a line of its own,
with its file, place and severity as named fields, among the lines saying what
the build wrote

`by/syntaxOutline` answers one document whole, on every revision the client wants
to serve features from: it is what an editor needs to type in an
indentation-delimited language — which line opens a suite, where a compound
statement ends, what a string literal's escapes and interpolations are, and how
much indentation a triple-quoted string has stripped. each string part also says
where its *text* is: the runs of the file whose text, joined, is what the literal
holds — one per line of a dedented string, past the indentation, and around the
interpolations of an f-string. that is what an editor needs to put another
language into a string it was not told about, with a place in the file for every
character of the fragment. the outline also carries the name each `def` and
`class` declares, a `def`'s parameter names, and on a `def` a class body makes,
`method`: `instance`, `class` or `static`, what its first parameter is passed —
from `@staticmethod` and `@classmethod`, `static def` and `class def`, and the
methods python makes so by name, `__new__` static (it is handed its class
explicitly, as its first argument) and `__init_subclass__` and `__class_getitem__`
class methods; a plain function has none. beside it, `receiver` says what the
first parameter is, the `self` or `cls` an editor writes when the parameter
list is opened: `instance` or `class`, `class` for `__new__` too, and absent for
any other static method.

a line broken inside an expression needs the expression in brackets, or a
backslash, to go on parsing. `wrap` names the expression Enter may break by
wrapping it in parentheses: on a simple statement, its one expression — an
assignment's value (`=`, an augmented one or an annotated one), a `return`'s, an
expression statement, a `raise` with no `from`, an `assert` with no message, a
`type` alias's value; on a clause, its header's — the condition of `if`, `elif`
and `while`, the iterable of `for`, the subject of `match`. each of these means the
same in parentheses, so an editor breaking a line inside one, outside any bracket
already there, writes `(` before the range and `)` after it. a statement with two
expressions (`raise E from cause`, `assert x, message`) has none, and nor does an
expression whose parentheses are syntax or that is not one in python's sense — a
basedpython anonymous named tuple, a callable or protocol type, a statement used as
an expression, a starred expression. and it carries the elements of every
comma-separated list of two or more — what an editor moves an argument among, or
tracks a declaration being renamed by

`.by` test files are collected under the same names as python ones — `test_*.by`
and `*_test.by` — because a `.by` transpiles to the `.py` of the same stem, and
that is the file pytest sees

### asking about the text you have

a client that keeps an answer against its own revision of a document can say
which text a document request is about, by adding `textHash` to the request's
params: FNV-1a, 64 bits, over the text's UTF-16 code units with every line ending
counted as one `\n`, written as sixteen lowercase hex digits. for a notebook cell
that is the cell's own text. the request is then answered about that text and no
other. when the server already holds it — the open buffer, or for
`by/syntaxOutline`, `by/injections`, `by/superMembers`,
`by/documentSuperMembers`, `by/transpile`, `textDocument/semanticTokens/full` and
`textDocument/documentSymbol` the file on disk of a document the client has not
opened — it answers at once; otherwise the
request waits for the `didOpen`, `didChange` or file write that brings the text,
and is answered with `ServerCancelled` if nothing has after ten seconds. so a
client need not know whether its `didOpen` has gone out before it asks, and an
answer never describes text it did not ask about. a request without `textHash` is
answered as before, and refused for a document that is not open

## debugger facts

while a program is stopped, an editor knows something no checker does: what the
names in the current frame actually hold. the server takes those readings and
answers what the code below the stop line will do, rather than what it could do

given a debugger stopped on line 5 holding `limit = 5`:

```by
def compute() -> int: ...

limit = compute()
# stopped here
if limit > 100:     # = false
    over = 1        # will not run
```

nothing in the source decides that branch — `compute()` returns an `int` and any
`int` is possible. the reading of `limit` is what settles it

this is the checker's ordinary reachability analysis, reading the file under one
extra assumption. it does not change the diagnostics you already see: a seeded
reading and an unseeded one are separate questions, and the ordinary one is what
the squiggles come from

the editor asks with a custom request, `by/dataFlowAt`:

```json
{
  "textDocument": { "uri": "file:///src/main.by" },
  "line": 5,
  "observations": [{ "name": "limit", "observed": "isInt", "text": "5" }]
}
```

`line` is one-based and is the line the program is stopped on. that line is
answered along with everything below it, because nothing on it has run yet.
a name may be a dotted path — `self.limit` — spelled as the source spells it

| `observed`     | carries                        | means                    |
| -------------- | ------------------------------ | ------------------------ |
| `isNone`       |                                | the value is `None`      |
| `isBool`       | `value`                        | exactly this `bool`      |
| `isInt`        | `text`, decimal                | exactly this integer     |
| `isStr`        | `text`                         | exactly this string      |
| `isBytes`      | `bytes`, an array of numbers   | exactly these bytes      |
| `isExactly`    | `module`, `qualname`           | `type(value)` is this    |
| `isEnumMember` | `module`, `qualname`, `member` | this member of this enum |

the answer is a list of findings, each with a `range`, a `kind` of `condition`
or `unreachable`, a `taken` for a condition, and a `label` to draw

an empty answer is the ordinary case. only readings the server can express as a
type produce anything, and only where nothing can have changed the name in
between:

- a binding at or below the stop line is the program's own assignment, and wins
    over a reading taken before it
- a name a loop around the stop line rebinds is refused, because the reading is
    true of this iteration and not of the next
- an observation applies only to the scope the program is actually stopped in.
    one frame's `limit` is not another's, and a name that scope does not itself
    bind — a global it only reads, an attribute it never assigns — is a value
    nothing in that scope can vouch for

### a value's type

a debugger names a value's type by its module and qualified name,
`type(value).__module__` and `__qualname__`: `shapes.base.Circle`, or
`shapes.base.Shape.Meta` for a class nested in another. `by/typeDeclaration`
takes `{"typeName": "shapes.base.Circle"}` and answers the class that names, as
`by/declaration` describes a declaration — `kind` (always `class`), `name`, its
`qualifiedName`, and `uri`, `range` and `selectionRange` — for an editor's *jump
to type source*. the name is resolved as an import of it would be: its first part
as a module of the project's environment, and each part after it as a member of
what came before, through submodules and re-exports, so `shapes.Circle`, where
`shapes/__init__` re-exports it, answers `shapes.base.Circle`. a class with a
source is answered at its source, and one with only a stub, as `builtins.list`,
at its stub. the answer is `null` for a name that reaches anything other than a
class, or nothing, as `make.<locals>.Local` does. a session of several projects
is answered from the first whose environment resolves it

## the expression under the cursor

a debugger's value tooltip, and *Evaluate Expression* or *Quick Evaluate* with
nothing selected, start from the expression the cursor is on. which expression
that is depends on the code rather than the characters: on `d` in
`a.b[c].d + compute(len(c))` it is `a.b[c].d`, on `b` it is `a.b`, and on `c`
it is `c`. the editor asks with `by/evaluatableExpression`:

```json
{
  "textDocument": { "uri": "file:///src/main.by" },
  "position": { "line": 1, "character": 19 },
  "calls": false
}
```

and the answer is the expression's `range`, or `null` where the position is not
on a name an expression ends with: a literal, an operator, a keyword, a comment,
or a name that a `def`, a class, an import or a keyword argument declares. a
parameter's name is the parameter. the program the debugger is stopped in is
python, so in a `.by` file an expression python would not read the same way,
like `user?.name`, is `null` too

`calls` says whether the expression may call anything, which is the difference
between hovering a name and asking for an evaluation. on `compute` the answer is
`compute` when it may not and `compute(len(c))` when it may, and a reference
whose value comes out of a call, like `make().d`, is answered only when it may.
reading an attribute or an item is answered either way, as every python
debugger's value tooltip does

## renaming a module

renaming `util.by` to `helpers.by` renames the module `alpha.util`, and every
`from alpha.util import thing` in the project now names a module that is not
there. an editor cannot find those on its own — it would have to resolve every
import against the same search paths the checker uses — so it asks first

the request is the protocol's own `workspace/willRenameFiles`, sent *before* the
file moves, and the answer is the edits that keep the project working. the
ordering is what makes the answer computable: the old path still holds the file,
so the module it is today can be resolved, and the new path is a path to read a
name out of

both a file and a *directory* are asked about, because renaming a directory
renames every module under it — the client sends only the directory, never its
contents

what gets rewritten:

```by
import alpha.util              # the dotted name
import alpha.util as util      # ... with an alias, which is left alone
from alpha.util import thing   # the module a symbol comes from
from alpha import util         # the module imported as a name
from .util import thing        # a relative import, after the dots
```

and the *uses* of a name an import binds, when that name changes.
`import alpha.util` binds `alpha`, so `alpha.util.thing()` in the body is part of
the rename too. those are found by asking what each expression is rather than by
matching text — a local called `util` in a file that also imports a module of
that name is not a reference to the module, and is not touched

a relative import inside a package that is being renamed as a whole comes out
unchanged, which is the truth: nothing about `from .util import thing` stops
working because its package was renamed around it

two things are deliberately not rewritten:

- **a module named as a string** — `importlib.import_module("alpha.util")`, an
    `INSTALLED_APPS` entry, an entry point in `pyproject.toml`. django's own
    module strings are handled by the django rename, and the rest are not
    distinguishable from any other string
- **an import that would have to change shape** — moving `alpha.util` to
    `beta.util` leaves `from alpha import util` needing a different statement,
    not a different word. those are reported in the server log rather than
    rewritten into something that does not mean the same thing
