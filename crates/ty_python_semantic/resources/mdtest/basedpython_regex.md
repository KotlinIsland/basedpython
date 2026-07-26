# regex group types

A regular expression written out as a literal tells us exactly how many capture groups it has, what
they are named, and which of them must have participated in any successful match. `re`'s stubs
cannot say any of that — `Match.group` is typed `AnyStr | None` and `findall` returns `list[Any]` —
so we read the pattern and refine the result.

## module-level functions

```py
import re

s = ""

if m := re.match("()?()", s):
    reveal_type(m.group(0))  # revealed: str
    reveal_type(m.group(1))  # revealed: str | None
    reveal_type(m.group(2))  # revealed: str
    reveal_type(m.groups())  # revealed: tuple[str | None, str]
```

## every match-producing function

```py
import re

s = ""

if m := re.search("()?()", s):
    reveal_type(m.groups())  # revealed: tuple[str | None, str]

if m := re.fullmatch("()?()", s):
    reveal_type(m.groups())  # revealed: tuple[str | None, str]

for m in re.finditer("()?()", s):
    reveal_type(m.groups())  # revealed: tuple[str | None, str]
```

## a pattern with no groups

```py
import re

if m := re.search("", ""):
    reveal_type(m.groups())  # revealed: tuple[()]
```

## subscripting a match

```py
import re

if m := re.match("()?()", ""):
    reveal_type(m[0])  # revealed: str
    reveal_type(m[1])  # revealed: str | None
    reveal_type(m[2])  # revealed: str
```

## compiled patterns

```py
import re

s = ""
p = re.compile("()?()")

if m := p.match(s):
    reveal_type(m.groups())  # revealed: tuple[str | None, str]

if m := p.search(s):
    reveal_type(m.groups())  # revealed: tuple[str | None, str]

if m := p.fullmatch(s):
    reveal_type(m.groups())  # revealed: tuple[str | None, str]

for m in p.finditer(s):
    reveal_type(m.groups())  # revealed: tuple[str | None, str]
```

## a narrowed pattern

Truthiness narrowing wraps the receiver in an intersection; the groups still come through it.

```py
import re

p = re.compile("()?()")
if p:
    if m := p.match(""):
        reveal_type(m.groups())  # revealed: tuple[str | None, str]
```

## bytes patterns

```py
import re

if m := re.match(b"()?()", b""):
    reveal_type(m.group(1))  # revealed: bytes | None
    reveal_type(m.group(2))  # revealed: bytes
    reveal_type(m.groups())  # revealed: tuple[bytes | None, bytes]
```

## a pattern behind a literal type

```py
from typing import Literal
import re

def f(r: Literal["()?()"]) -> None:
    if m := re.search(r, ""):
        reveal_type(m.groups())  # revealed: tuple[str | None, str]
```

## named groups

```py
import re

if m := re.match("(?P<a>a)(?P<b>b)?", "a"):
    reveal_type(m.group("a"))  # revealed: str
    reveal_type(m.group("b"))  # revealed: str | None
```

## several groups at once

```py
import re

if m := re.match("(a)(b)?", "a"):
    reveal_type(m.group(1, 2))  # revealed: tuple[str, str | None]
```

## groupdict

```py
import re

if m := re.match("(?P<a>a)(?P<b>b)?", "a"):
    d = m.groupdict()
    reveal_type(d["a"])  # revealed: str
    reveal_type(d["b"])  # revealed: str | None
    d["a"] = ""
    reveal_type(m.groupdict(1))  # revealed: <TypedDict with items 'a', 'b'>
    reveal_type(m.groupdict(1)["b"])  # revealed: str | Literal[1]
```

## groupdict on a pattern with no names

The dict is always empty, and an empty `TypedDict` would say nothing extra while costing the caller
everything a `dict` can be passed to, so the stub's own answer stands.

```py
import re

if m := re.match("(a)(b)?", "a"):
    reveal_type(m.groupdict())  # revealed: dict[str, str | None]
```

## a default for unmatched groups

```py
import re

if m := re.match("()?()", ""):
    reveal_type(m.groups(1))  # revealed: tuple[str | Literal[1], str]
```

## findall

```py
import re

reveal_type(re.findall("abc", "abc"))  # revealed: list[str]
reveal_type(re.findall("(a)bc", "abc"))  # revealed: list[str]
reveal_type(re.findall("(a)(b)c", "abc"))  # revealed: list[tuple[str, str]]
reveal_type(re.findall("(a)(b)(c)", "abc"))  # revealed: list[tuple[str, str, str]]
# a group that did not participate comes back as the empty string, not `None`
reveal_type(re.findall("(a)(b)?(c)", "ac"))  # revealed: list[tuple[str, str, str]]
reveal_type(re.findall(b"", b""))  # revealed: list[bytes]
```

## split

```py
import re

reveal_type(re.split("abc", "abc"))  # revealed: list[str]
reveal_type(re.split("(a)bc", "abc"))  # revealed: list[str]
reveal_type(re.split("(a)(b)c", "abc"))  # revealed: list[str]
reveal_type(re.split("(a)(b)?(c)", "ac"))  # revealed: list[str | None]
```

## sub and subn

The callable replacement is handed a `Match` for the same pattern, so it sees the groups too.

```py
import re

r = re.sub("()?()", lambda m: str(reveal_type(m.groups())), "")  # revealed: tuple[str | None, str]
reveal_type(r)  # revealed: str

n = re.subn("()?()", lambda m: str(reveal_type(m.groups())), "")  # revealed: tuple[str | None, str]
reveal_type(n)  # revealed: tuple[str, int]

c = re.compile("()?()").sub(lambda m: str(reveal_type(m.groups())), "")  # revealed: tuple[str | None, str]
reveal_type(c)  # revealed: str
```

## a pattern we cannot see

Nothing is refined, and the stub's own answer stands — which, unlike upstream typeshed's `Any`,
still says out loud that a group may not have participated.

```py
import re

def f(pattern: str) -> None:
    p = re.compile(pattern)
    if m := p.match(""):
        reveal_type(m.groups())  # revealed: tuple[str | None, ...]

def g(m: re.Match[str]) -> None:
    reveal_type(m.group(0))  # revealed: str
    reveal_type(m.group(1))  # revealed: str | None
    reveal_type(m[1])  # revealed: str | None
    reveal_type(m.groups())  # revealed: tuple[str | None, ...]
```

## groups do not leak between patterns

```py
import re

m = re.match("()?()", "")
m = re.match("()", "")
if m:
    reveal_type(m.groups())  # revealed: tuple[str]
```

## a pattern that reaches an annotated variable

```py
from typing import Final
import re

p1: re.Pattern[str] = re.compile("()")
if m := p1.match(""):
    reveal_type(m[1])  # revealed: str

p2: Final[re.Pattern[str]] = re.compile("()")
if m := p2.match(""):
    reveal_type(m[1])  # revealed: str
```

## two patterns joining at a branch

Neither pattern's groups hold for the union, so the refinement drops away rather than picking one.

```py
import re

def f(flag: bool) -> None:
    m = re.match("(a)", "") if flag else re.match("(a)?", "")
    if m:
        reveal_type(m.groups())  # revealed: tuple[str | None, ...]
```

## the verbose flag

Verbose mode changes what the pattern parses to, so it has to be accounted for — and where the flags
cannot be read statically, nothing is refined at all rather than guessed.

```py
import re
from re import X

if m := re.match("()#()", "", X):
    reveal_type(m.groups())  # revealed: tuple[str]

if m := re.match("()#()", "", re.X):
    reveal_type(m.groups())  # revealed: tuple[str]

if m := re.match("()#()", "", re.VERBOSE):
    reveal_type(m.groups())  # revealed: tuple[str]

if m := re.match("()#()", "", flags=re.X):
    reveal_type(m.groups())  # revealed: tuple[str]

if m := re.match("()#()", "", flags=re.X | re.DOTALL):
    reveal_type(m.groups())  # revealed: tuple[str]

if m := re.match("()#()", "", flags=re.DOTALL):
    reveal_type(m.groups())  # revealed: tuple[str, str]

if m := re.match("()#()", "", flags=re.X & re.DOTALL):
    reveal_type(m.groups())  # revealed: tuple[str | None, ...]

def f(flags: int) -> None:
    if m := re.match("()#()", "", flags):
        reveal_type(m.groups())  # revealed: tuple[str | None, ...]
```

## a pattern that turns verbose on itself

```py
import re

if m := re.match("(?x)()#()", ""):
    reveal_type(m.groups())  # revealed: tuple[str]
```

## invalid patterns

```py
import re

re.compile("(")  # error: [invalid-regex] "missing ), unterminated subpattern at position 0"
re.search("(", "")  # error: [invalid-regex] "missing ), unterminated subpattern at position 0"
re.match("(", "")  # error: [invalid-regex] "missing ), unterminated subpattern at position 0"
re.fullmatch("(", "")  # error: [invalid-regex] "missing ), unterminated subpattern at position 0"
re.finditer("(", "")  # error: [invalid-regex] "missing ), unterminated subpattern at position 0"
re.split("(", "")  # error: [invalid-regex] "missing ), unterminated subpattern at position 0"
re.findall("(", "")  # error: [invalid-regex] "missing ), unterminated subpattern at position 0"
re.sub("(", "", "")  # error: [invalid-regex] "missing ), unterminated subpattern at position 0"
```

## a group the pattern has not got

```py
import re

if m := re.match("()?()", ""):
    reveal_type(m.group(2))  # revealed: str
    # error: [invalid-regex] "No such group: 3"
    reveal_type(m.group(3))  # revealed: Unknown
    # error: [invalid-regex] "No such group: 3"
    reveal_type(m[3])  # revealed: Unknown
```

## naming a group without taking its value

`start`, `end` and `span` raise the same `IndexError` for a group the pattern has not got.

```py
import re

if m := re.match("()?()", ""):
    reveal_type(m.start(2))  # revealed: int
    m.start(3)  # error: [invalid-regex] "No such group: 3"
    m.end(3)  # error: [invalid-regex] "No such group: 3"
    m.span(3)  # error: [invalid-regex] "No such group: 3"
```

## a named group the pattern has not got

```py
import re

if m := re.match("(?P<a>)", ""):
    reveal_type(m.group("a"))  # revealed: str
    # error: [invalid-regex] "No such group: 'b'"
    reveal_type(m.group("b"))  # revealed: Unknown
```
