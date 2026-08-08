# Celery

celery ships no `py.typed`, and `shared_task` is `def shared_task(*args, **kwargs)` in the source,
so without stubs a decorated task is `Unknown` and every `.delay()` call goes unchecked. The
`celery-types` package supplies `Task[_P, _R]` — the same whole-callable capture the vendored
`functools.byi` uses for `_lru_cache_wrapper` — and `delay` re-exposes the task function's own
parameters. These tests pin that recovery, because it is what makes a mistyped or miscounted
`.delay()` argument an error rather than silence.

```toml
[environment]
python-version = "3.13"
python-platform = "linux"

[project]
dependencies = ["celery==5.6.3", "celery-types==0.26.0"]
```

## A bare `@shared_task` keeps its signature

`.delay` takes exactly what the task function takes, and the result carries the return type.

```py
from celery import shared_task

@shared_task
def process(book_id: int, force: bool = False) -> str:
    return f"{book_id}{force}"

reveal_type(process)  # revealed: Task[(book_id: int, force: bool = False), str]
reveal_type(process.delay(1))  # revealed: AsyncResult[str]
reveal_type(process.delay(1).get())  # revealed: str

process.delay(1, True)
process.delay(1, force=True)

# error: [invalid-argument-type] "Argument to bound method `Task.delay` is incorrect: Expected `int`, found `Literal["not an int"]`"
process.delay("not an int")

# error: [missing-argument] "No argument provided for required parameter `book_id` of bound method `Task.delay`"
process.delay()

# error: [invalid-argument-type]
# error: [too-many-positional-arguments]
process.delay(1, 2, 3, 4)

# error: [unknown-argument] "Argument `nope` does not match any known parameter of bound method `Task.delay`"
process.delay(1, nope=True)
```

## `@shared_task(...)` with arguments keeps its signature

The called form resolves through a different overload, so it is worth pinning separately.

```py
from celery import shared_task

@shared_task(name="x", rate_limit="10/m")
def named(a: int) -> bytes:
    return b""

reveal_type(named)  # revealed: Task[(a: int), bytes]
reveal_type(named.delay(1))  # revealed: AsyncResult[bytes]

# error: [invalid-argument-type]
named.delay("nope")
```

## `bind=True` does not leak `self` into `.delay`

celery prepends the task instance to the task function, and callers must not pass it. Confirmed
against celery 5.6.3: `inspect.signature(bound.run)` is `(a: int) -> str`, with no `self`.

```py
from celery import shared_task

@shared_task(bind=True, max_retries=5)
def flaky(self, url: str, attempts: int = 3) -> bytes:
    return url.encode()

reveal_type(flaky)  # revealed: Task[(url: str, attempts: int = 3), bytes]
reveal_type(flaky.delay("http://x"))  # revealed: AsyncResult[bytes]

flaky.delay("http://x", 1)

# passing the task instance explicitly is the mistake `bind=True` invites
# error: [invalid-argument-type]
# error: [invalid-argument-type]
flaky.delay(flaky, "http://x")
```

## Calling a task directly still runs it synchronously

A task is callable, and that path keeps the function's own return type.

```py
from celery import shared_task

@shared_task
def add(x: int, y: int) -> int:
    return x + y

reveal_type(add(1, 2))  # revealed: int

# error: [invalid-argument-type]
add("a", 2)
```

## The rest of a task's surface survives

`apply_async` takes a packed tuple and dict rather than the parameters spread, so its arguments stay
`Any`; the result type is still recovered. `si` is parameter-checked, `s` is not — it is used for
partial application in a chain.

```py
from celery import shared_task

@shared_task
def add(x: int, y: int) -> int:
    return x + y

reveal_type(add.apply_async((1, 2)))  # revealed: AsyncResult[int]
reveal_type(add.apply_async(args=(1, 2), countdown=10))  # revealed: AsyncResult[int]
reveal_type(add.s(1))  # revealed: Signature[int]
reveal_type(add.si(1, 2))  # revealed: Signature[int]
reveal_type(add.name)  # revealed: str
reveal_type(add.request)  # revealed: Context
reveal_type(add.max_retries)  # revealed: int | None

# `si` is spread, so it is checked
# error: [invalid-argument-type]
add.si("a", 2)
```

## A task imported from another module keeps its signature

`tasks.py`:

```py
from celery import shared_task

@shared_task
def add(x: int, y: int) -> int:
    return x + y
```

`consumer.py`:

```py
from tasks import add

reveal_type(add)  # revealed: Task[(x: int, y: int), int]

# error: [invalid-argument-type]
add.delay("a", 2)
```

## `@app.task` refuses rather than guesses

`Celery` is generic over the task *class* so that `task_cls=` and `base=` work, and `celery-types`
declares `Celery.task` as returning that same class variable. It therefore cannot also carry the
task function's parameters, and `app.task` widens to `Task[(...), Any]`. That is a refusal, not a
wrong answer: the calls below are unchecked, exactly as they are without stubs, and nothing correct
is rejected.

```py
from celery import Celery

app = Celery("probe")

@app.task
def emit(topic: str, body: str = "") -> None: ...

reveal_type(app)  # revealed: Celery[Task[(...), Any]]
reveal_type(emit)  # revealed: Task[(...), Any]
reveal_type(emit.delay("t"))  # revealed: AsyncResult[Any]

# unchecked, and knowingly so
emit.delay(1, 2, 3)

@app.task(bind=True)
def emit_bound(self, topic: str) -> None: ...

reveal_type(emit_bound)  # revealed: Task[(...), Any]
```
