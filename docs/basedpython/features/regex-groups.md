# regex group types

a regular expression written out as a literal says exactly how many capture
groups it has, what they are named, and which of them must have participated in
any successful match. the `re` stubs cannot express any of that — every group is
typed the same way whatever the pattern — so basedpython reads the pattern and
types the result from it:

```py
import re

if m := re.match("()?()", text):
    reveal_type(m[1])        # str | None
    reveal_type(m[2])        # str
    reveal_type(m.groups())  # tuple[str | None, str]
```

group 2 is inside no optional construct, so it is set whenever the match
succeeded; group 1 sits under a `?`, so it may not be.

this is an enhancement with no new syntax — it applies to `.py` files as well as
`.by`.

## where the groups come from

a pattern is read wherever it reaches `re` as a literal, and the groups travel
with the compiled pattern:

```py
p = re.compile(r"(?P<key>\w+)=(?P<value>.*)")

if m := p.search(line):
    reveal_type(m.group("key"))    # str
    reveal_type(m.groupdict())     # a TypedDict with 'key' and 'value'
```

`match`, `fullmatch`, `search` and `finditer` all carry the groups onto the
`Match` they produce, and `sub` / `subn` pass them to a callable replacement:

```py
re.sub("()?()", lambda m: str(m.groups()), text)  # m.groups() is tuple[str | None, str]
```

## the whole surface

| expression           | with `"(a)(b)?"`                    |
| -------------------- | ----------------------------------- |
| `m[0]`, `m.group(0)` | `str`                               |
| `m[1]`, `m.group(1)` | `str`                               |
| `m[2]`, `m.group(2)` | `str \| None`                       |
| `m.group(1, 2)`      | `tuple[str, str \| None]`           |
| `m.groups()`         | `tuple[str, str \| None]`           |
| `m.groups(0)`        | `tuple[str, str \| Literal[0]]`     |
| `m.groupdict()`      | a `TypedDict` over the named groups |
| `re.findall(...)`    | `list[tuple[str, str]]`             |
| `re.split(...)`      | `list[str \| None]`                 |

a `bytes` pattern gives `bytes` throughout.

`groupdict()` keeps the stub's plain `dict` when the pattern names no groups: an
empty `TypedDict` would say nothing extra about a dict that is always empty,
while costing the caller everything a `dict` can be passed to.

`findall` is the one place a group that did not participate is reported as the
empty string rather than `None`, so its elements are never optional.

## when a group is guaranteed

a group is set on every successful match unless something on the way to it could
have been skipped:

```py
"(a)"           # set
"(a)+"          # set — the repeat matches at least once
"(a)?"          # optional
"(a)*"          # optional
"(a){0,3}"      # optional
"(a)|(b)"       # both optional — only one branch ran
"(?:(a))"       # set — a non-capturing group changes nothing
"(?=(a))"       # set — a positive lookahead does capture
"(?!(a))"       # optional — if the match succeeded, this did not
"(?(1)(a)|(b))" # both optional — the condition is not evaluated statically
```

verbose mode changes what a pattern even parses to, so the `flags` argument is
read too — `re.X`, `re.VERBOSE`, a `|` chain containing either, or a pattern
that turns it on itself with `(?x)`. where the flags cannot be read statically,
nothing is refined rather than guessed.

## patterns we cannot see

a pattern built at runtime keeps the stub's own answer, which in basedpython
still says out loud that a group may not have participated — upstream typeshed
types those positions as `Any`, which silently hides the `None`:

```py
def f(pattern: str, text: str) -> None:
    if m := re.match(pattern, text):
        reveal_type(m.group(1))  # str | None
```

## diagnostics

the same reading of the pattern catches two mistakes that otherwise only fail at
runtime, both reported as `invalid-regex`:

```py
re.compile("(")  # error: missing ), unterminated subpattern at position 0

if m := re.match("(a)(b)", text):
    m.group(3)   # error: No such group: 3
```

the group check also covers `m[3]`, `m.start(3)`, `m.end(3)` and `m.span(3)`.

a construct the pattern reader does not model produces neither a refinement nor
a diagnostic: a pattern reported invalid when `re.compile` would have accepted it
is much worse than one left alone.
