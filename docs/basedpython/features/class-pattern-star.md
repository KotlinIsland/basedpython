# starred wildcards in class patterns

`*_` inside a class pattern stands for the positions the pattern does not name,
so whatever follows it reads the *last* of them:

```by
class Line:
    __match_args__ = ("start", "mid", "stop", "end")

match line:
    case Line(a, *_, b):
        print(a, b)  # line.start and line.end
```

transpiles to:

```python
match line:
    case Line(a, _, _, b):
        print(a, b)
```

a class pattern's positions are the names the class lists in `__match_args__`,
in order. python only counts them from the front, so reaching the last one means
writing out every position before it and keeping that count in step with the
class. the star says "however many are left" instead

## where the star can go

anywhere among the positional subpatterns, and at most once. a pattern with two
of them would leave the positions between them unplaceable, and both cases are
rejected as syntax errors:

```by
case Line(*_, b)          # b reads the last position
case Line(a, *_, b, c)    # b and c read the last two
case Line(*_, a, *_, b)   # rejected: only one starred subpattern
```

the star is a positional subpattern, so like any other it has to come before the
keyword ones. keywords name their attribute outright and are unaffected by what
the star did to the positions:

```by
case Line(a, *_, mid=m)
```

## the star never binds

it is the wildcard, spelled `*_`. a class pattern has no sequence behind it —
only the fixed names `__match_args__` lists — so there is nothing for a starred
capture to collect, and `case Line(a, *rest)` is rejected

## a trailing star

python already lets a class pattern name fewer positions than the class has, so
`case Line(a, *_)` accepts exactly what `case Line(a)` accepts. writing it is a
way of saying out loud that the rest was deliberately ignored; it lowers to the
pattern without it:

```by
case Line(a, *_)
```

transpiles to:

```python
case Line(a)
```

a lone `case Line(*_)` becomes `case Line()`, which matches any `Line`

## when the class is not specific enough

placing a subpattern after the star means counting back from the length of
`__match_args__`, so that length has to be known. a class whose
`__match_args__` is widened to `tuple[str, ...]`, or absent, or conditionally
defined, does not say what its last position is, and the pattern is reported as
`invalid-match-pattern`:

```by
class Variadic:
    __match_args__: tuple[str, ...] = ()

case Variadic(a, *_, b)  # error: cannot place `*_`
```

a trailing star is fine on the same class, since nothing after it needs a
position

## related

the star is part of the pattern language, so it works wherever a pattern does —
a `match` case, a [`let` binder](destructuring.md), an
[`if let` clause](if-let.md), a `for` target, or a parameter
