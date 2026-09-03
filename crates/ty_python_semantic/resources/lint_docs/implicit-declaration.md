## What it does

Checks for a variable that a basedpython file assigns without ever declaring it.

## Why is this bad?

Python introduces a variable by assigning to it, so a typo makes a new variable
rather than an error, and reading a statement tells you nothing about whether
the name is new or one you have seen before.

basedpython has a keyword for each: `let` for a binding that never changes, and
`var` for one that does. With this rule on, every variable a scope binds has to
be declared once with one of them, and every later assignment is visibly a
re-assignment.

This rule is off by default, because a file written without the keywords is
valid basedpython.

## Examples

Every assignment to a name the scope never declares is reported, so a variable
introduced this way is reported wherever it is written:

`undeclared.by`:

```by
count = 0  # error: [implicit-declaration]
count = count + 1  # error: [implicit-declaration]
```

Declaring it once answers all of them:

`declared.by`:

```by
var count = 0
count = count + 1
```

An assignment to something other than a plain name — an attribute, a subscript,
an item of an unpacking — is not a declaration, and is never reported.
