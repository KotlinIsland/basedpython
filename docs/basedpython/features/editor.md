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

| template      | rewrites `x` into |
| ------------- | ----------------- |
| `.print`      | `print(x)`        |
| `.not`        | `not x`           |
| `.type`       | `type(x)`         |
| `.repr`       | `repr(x)`         |
| `.list`       | `list(x)`         |
| `.par`        | `(x)`             |
| `.return`     | `return x`        |
| `.raise`      | `raise x`         |
| `.if`         | an `if` statement |
| `.for`        | a `for` loop      |
| `.match`      | a `match`         |
| `.let` `.var` | a binding         |

the templates from `.return` down are statements, so they are only offered
where the expression is the whole statement — `f(xs.print)` offers `.print`
but not `.for`. the ones that open a suite or need a name to bind place the
cursor for you, when your editor supports snippets

`.let` and `.var` spell basedpython bindings; the rest are valid python and are
offered in `.py` files too

### postfix `await`

[`.await`](await-attribute.md) is not a template — it is real syntax, so it
completes as an ordinary word. it appears inside an `async def`, on an
expression that can actually be awaited

## completions

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
the caret between them, ready for the first argument. set
`ty.completions.completeFunctionParentheses` to `false` to insert the bare name
instead

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

## refactorings

the server offers refactorings as code actions. each is worked out from what the
checker reads the file to mean, not from its text: inlining `x` replaces the reads
of that variable, never a same-named parameter of another function, an attribute
`obj.x` or a keyword argument `f(x=1)`

a refactoring either keeps the program meaning what it meant or is refused, and a
refusal says why. an editor that asks for refactorings explicitly shows the reason;
one that asks while the caret moves is only offered what applies

| refactoring             | kind                                | offered on                        |
| ----------------------- | ----------------------------------- | --------------------------------- |
| inline variable         | `refactor.inline.variable`          | a variable assigned once          |
| extract variable        | `refactor.extract.variable`         | a selected expression             |
| introduce constant      | `refactor.extract.constant`         | a selected expression of literals |
| extract function        | `refactor.extract.function`         | selected statements               |
| add return annotation   | `refactor.rewrite.returnAnnotation` | a `def` header with no `->`       |
| convert to data class   | `refactor.rewrite.toDataClass`      | a class header                    |
| convert from data class | `refactor.rewrite.fromDataClass`    | a `data class` header             |
| put on separate lines   | `refactor.rewrite.splitList`        | a list not yet one element a line |
| put on one line         | `refactor.rewrite.joinList`         | a list spread over several lines  |

moving an expression can change when it runs, so that is what most refusals are
about. a value with an effect is only inlined into a single read in the very next
statement, with nothing that could observe the move evaluated before it; an
expression is only extracted above its statement when it runs exactly once each
time the statement does — not in a loop condition, a conditional branch, a lambda
or after a call

```by
while queue.pending():  # extracting `queue.pending()` is refused: it runs every iteration
    queue.pop()
```

adding a return annotation writes the return type the checker infers, importing
what it names. it is refused when that type is partly `Unknown` — `x + y` of two
unannotated parameters — since `Unknown` is no type a program can name, and the
rest of the union would claim more than the body says. an inlay hint's
double-click edit follows the same rule

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

putting a list on separate lines, or on one, rewrites the innermost list with
brackets of its own around the caret — a call's arguments, a definition's
parameters, a display's elements — and only the text between its elements. an
element is a node of the parse, so a lambda among the arguments keeps its commas.
a split list ends with a comma, which tells a formatter it is meant to stay split,
and a joined one loses it unless it is a tuple of one. a list with a comment in it
is refused, since joining would put what follows the comment inside it

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

besides the standard modifiers, a token can carry `functionScope`: the name is
bound in the scope of a function, a lambda or a comprehension — a parameter or a
local, as against a module global or a class attribute. it is the binding that
decides, not where the name is written: a global read inside a function is not
marked, a `nonlocal` name is, and an assignment expression in a comprehension at
the top of a module binds a global. an editor that colours each local by its name
("rainbow" highlighting) reads it off this modifier

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

## go to super

`textDocument/implementation` on a method goes down, to the methods that
override it. a decorated override is its own `def`, not the function its
decorator returns, which a decorator like `logged`, returning a `wrapper` of its
own, defines somewhere else. `by/superMembers` goes up: given a position on the name a class
member is declared with — a `def` or a nested class, a name the class body
assigns, annotates or captures, or an import's alias — it answers the
superclass members that member overrides, each with its class, its file and the
name to land on. a class goes up with `typeHierarchy/supertypes`

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
classes. a dunder is left out, being called by the language rather than by
name, and so is an `extension` block, which names a class it does not declare

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
`class` declares and a `def`'s parameter names, and the elements of every
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
