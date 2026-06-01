# destructuring with `if let`

> **status: planned** — not yet implemented; this page is a design sketch

shorthand for a one-variant peel of an enum or optional value:

```by
if let Some(x) := opt:
    use(x)
```

equivalent to a single-arm `match` followed by the else branch. extends the
walrus operator with pattern syntax: the pattern left of `:=` is any `match`
pattern, the condition is true when it matches, and its captures are bound in
the body

```by
if let Shape.Circle(r) := shape:
    print(r * 2)
else:
    print("not a circle")
```

## open questions

- whether captures should leak past the `if` (walrus bindings do; `match`
    captures effectively do too — leaking is the consistent choice)
- `while let` for loop-and-peel iteration
