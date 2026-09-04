# basedpython-ui: `escaping-loop-variable` sees through `once` blocks

A `once` block runs inline, exactly once, so it never lets a captured loop variable dangle — and it
is not where the check stops. A handler block nested inside it whose callee is not a borrow may be
kept and run after the loop advances, when the variable holds its final value: the classic late
binding trap, one level down. A compose-style tree nests exactly like this — a keyed `once` scope
around a widget whose click handler reads the loop item.

```toml
[environment]
python-version = "3.12"
```

## a handler block inside a `once` block inside a loop is flagged

```by
def key(k: object, once content: () -> None):
    content()

def Button(label: str, on_click: () -> None): ...

for item in ("a", "b"):
    key(item):
        Button(item):
            print(item)  # error: [escaping-loop-variable]
```

## the nesting may be deeper

```by
def key(k: object, once content: () -> None):
    content()

def group(once content: () -> None):
    content()

def Button(label: str, on_click: () -> None): ...

for item in ("a", "b"):
    key(item):
        group:
            Button(item):
                print(item)  # error: [escaping-loop-variable]
```

## a `once` handler nested in a `once` block is confined

```by
def key(k: object, once content: () -> None):
    content()

def run(once fn: () -> None):
    fn()

for item in ("a", "b"):
    key(item):
        run:
            print(item)
```

## a nested handler that does not capture the loop variable is fine

```by
def key(k: object, once content: () -> None):
    content()

def Button(label: str, on_click: () -> None): ...

for item in ("a", "b"):
    key(item):
        Button("static"):
            print("clicked")
```

## a nested handler outside any loop is fine

```by
def key(k: object, once content: () -> None):
    content()

def Button(label: str, on_click: () -> None): ...

item = "a"
key(item):
    Button(item):
        print(item)
```
