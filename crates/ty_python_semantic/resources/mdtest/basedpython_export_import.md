# basedpython: `from x export y`

`from x export y` is the re-exporting form of `from x import y`. it means exactly what python spells
`from x import y as y`: the name is bound, and it is deliberately part of the importing module's
public api rather than an implementation detail.

that distinction only bites in stub files, where a plain `import` is private to the stub and
importing through it is an error.

## a stub re-exports what it exports

`b.byi`:

```byi
from typing export Any
```

`main.by`:

```by
from b import Any

reveal_type(Any)  # revealed: <special-form 'typing.Any'>
```

## a plain import in a stub stays private

the contrast case: without `export`, `b` has no public `Any`.

`b.byi`:

```byi
from typing import Any
```

`main.by`:

```by
# error: [unresolved-import] "Module `b` has no member `Any`"
from b import Any

reveal_type(Any)  # revealed: Unknown
```

## every name in the statement is re-exported

`b.byi`:

```byi
from c export first, second
```

`c.byi`:

```byi
first: int
second: str
```

`main.by`:

```by
from b import first, second

reveal_type(first)  # revealed: int
reveal_type(second)  # revealed: str
```

## a relative export re-exports too

`pkg/__init__.byi`:

```byi
from .impl export Widget
```

`pkg/impl.byi`:

```byi
class Widget: ...
```

`main.by`:

```by
from pkg import Widget

reveal_type(Widget())  # revealed: Widget
```

## the exported name is bound locally as well

`export` binds the name in the importing module like any other import.

`b.byi`:

```byi
class Widget: ...
```

`main.by`:

```by
from b export Widget

reveal_type(Widget())  # revealed: Widget
```

## `export` is not valid in a `.py` file

```py
# error: [invalid-syntax] "`from ... export ...` is not valid in `.py` files"
from typing export Any
```

## `export` cannot rename

renaming contradicts the keyword: `export` binds each name under itself.

```by
# error: [invalid-syntax] "`export` cannot be combined with an `as` clause; use `from ... import ... as ...` instead"
from typing export Any as A
```

## `export` cannot star

```by
# error: [invalid-syntax] "`export` cannot be used with a star import"
from typing export *
```
