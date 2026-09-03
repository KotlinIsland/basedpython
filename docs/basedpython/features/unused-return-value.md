# unused return value

a call written as a statement of its own keeps whatever the call did on the way to its answer, and
throws the answer away. `unused-return-value` reports that

```python
def parse(text: str) -> int: ...

parse("1")  # warning: the parsed number goes nowhere
```

the classic instance is a method that looks like it changes its receiver and does not — `str` is
immutable, so the stripped string was the whole result

```python
def clean(text: str) -> str:
    text.strip()  # warning: strips nothing
    return text
```

a function that returns `None` has no answer to throw away, so nothing is reported

```python
def log(message: str) -> None: ...

log("started")  # ok
```

neither is a call that does not come back at all (`-> Never`), nor one whose result is gradual —
there is nothing known to have been discarded

## opting out

some results really are optional. `@ignorable_return_value` says so, and calls to that function stop
being reported

```by
@ignorable_return_value
def prime_cache(key: str) -> bytes: ...

prime_cache("k")  # ok — the call was for the caching
```

the marker is available without importing it, like the rest of basedpython's vocabulary, and it
leaves no trace in the emitted python — nothing runs, the checker just reads it

on a class it covers every method the class body defines, and constructing the class. that is the
shape of a fluent builder, where each step answers with the receiver it was given

```by
@ignorable_return_value
class Query:
    def where(self, clause: str) -> Query:
        return self

    @must_use_return_value
    def rows(self) -> list[str]:
        return []

q = Query()
q.where("id = 1")  # ok — the class is marked
q.rows()           # warning: `rows` is marked back
```

`@must_use_return_value` is the only way back inside a marked class. on a declaration that carries
both markers it wins, since it is the more specific of the two

## the standard library

the stdlib is checked like anything else, and the members whose result is idiomatically thrown away
carry the marker in basedpython's own stubs

```python
entries = [1, 2]
entries.pop()               # ok — `pop` is for the removing
handle.write("done")        # ok — the byte count is for a partial write
subprocess.run(["ls"])      # ok
"done".strip()              # warning
```

the bar for a member is that discarding must be *idiomatic*, not merely common: `path.read_text()`
discarded is a bug, `path.write_text(...)` discarded is how it is written

## coroutines

a coroutine that reaches the end of a statement is missing its `await`, which is
`unused-awaitable`'s to report — so it is not reported twice. once awaited, what the coroutine
answered with is an ordinary result

```python
async def fetch() -> bytes: ...

async def main() -> None:
    fetch()        # warning: object of type `Coroutine[...]` is not awaited
    await fetch()  # warning: the bytes go nowhere
```
