# assignment alignment

the formatter lines up the `=` of consecutive assignments:

```by
alpha      = 1
beta      += 2
gamma: int = 3
```

every kind of assignment takes part — plain assignments, unpacking targets,
augmented assignments, annotated assignments, chained targets, and the
basedpython declaration keywords:

```by
name            = "a"
count          += 1
total: int      = 0
first, second   = 1, 2
[third, fourth] = 3, 4
*head, tail     = [1, 2, 3]
matrix[0]       = 5
let declared    = 6
```

what lines up is the `=` that introduces the value — the whole left side of the
assignment sits to the left of it, and every value in the run sits to the right.
an augmented assignment is padded in front of its operator, so the `=` of a
`//=` lands in the same column as the `=` of a plain assignment. a chained
`a = b = 1` aligns its *last* `=`, the one before the value; the targets before
it keep their single space:

```by
chained = also              = 1
deep = chain = of = targets = 2
plain                       = 3
```

## runs

a *run* is a stretch of assignments that nothing interrupts. a run ends at

- an empty line — one that survives formatting. an empty line the formatter drops,
    such as the one after a statement that ends in a semicolon, doesn't end a run
- a statement that isn't an assignment, including a bare annotation such as
    `declared: int`, which has no `=`
- a statement whose formatting is suppressed with `# fmt: off` or `# fmt: skip`.
    it's printed exactly as written, so the assignments around it are left to line
    up among themselves

a comment on its own line doesn't end a run — put a blank line above it to
start a new one:

```by
alpha = 1
# still the same run
beta  = 2

gamma       = 1
much_longer = 2
```

each body is aligned on its own, so a nested function or class never widens the
assignments around it

## which assignments are left out

an assignment that can't be lined up is left out of its run; the ones around it
still line up with each other. that happens to

- an assignment whose left side the formatter splits over several lines, whether
    because of a magic trailing comma or because it doesn't fit. there's no single
    column to line up to, and a left side that has been split once tends to stay
    split, so counting it would make the alignment depend on how many times the
    file has been formatted
- an assignment whose left side leaves no room for its value within the line
    width. padding the others out to it would push their values past the margin

a run of one is left as it is, since a lone assignment is already lined up with
itself

## configuration

alignment is on by default, in `.py` files as much as in `.by` files — it's a
deliberate deviation from black. to turn it off:

```toml
[tool.ruff.format]
assignment-alignment = "disabled"
```

it conflicts with `E221` (`multiple-spaces-before-operator`), which lints against
the very spaces the alignment inserts. enabling that rule while the formatter
aligns produces a warning; disable one or the other

the padding is what the `multiple-spaces-before-operator` (`E221`) lint rule
complains about, and its fix would take the alignment straight back out. disable
that rule where assignments are aligned — `by format` warns when both are on
