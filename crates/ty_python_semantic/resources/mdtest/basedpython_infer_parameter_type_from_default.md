# basedpython: infer parameter type from default

by default, an unannotated parameter follows the gradual guarantee: it is left unannotated even when
it has a default value, so any argument is accepted at the call site.

the `analysis.infer-parameter-type-from-default` option deliberately breaks that guarantee. when it
is enabled, an unannotated parameter with a default is declared with the default's promoted type, so
`def f(a=1)` treats `a` as `int` (not `Literal[1]`, and not gradual).

this is a basedpython enhancement that also applies to plain python files.

## enabled

```toml
[environment]
python-version = "3.12"

[analysis]
infer-parameter-type-from-default = true
```

the parameter's type inside the body is the promoted type of the default:

```py
def f(a=1):
    reveal_type(a)  # revealed: int

def g(a="s", b=True):
    reveal_type(a)  # revealed: str
    reveal_type(b)  # revealed: bool
```

the signature is checked at call sites: an incompatible argument is now an error:

```py
f(2)  # ok
f("x")  # error: [invalid-argument-type]

g("hello", False)  # ok
g(1, True)  # error: [invalid-argument-type]
```

a parameter with no default is unaffected and stays gradual:

```py
def h(a, b=1):
    reveal_type(a)  # revealed: Unknown
    reveal_type(b)  # revealed: int

h("anything", 2)  # ok
```

an explicit annotation always wins over the default:

```py
def annotated(a: str = "s"):
    reveal_type(a)  # revealed: str

annotated("t")  # ok
annotated(1)  # error: [invalid-argument-type]
```

## disabled (default)

```toml
[environment]
python-version = "3.12"
```

with the option off, the gradual guarantee holds: an unannotated parameter with a default stays
gradual and accepts any argument.

```py
def f(a=1):
    reveal_type(a)  # revealed: Unknown | Literal[1]

f("x")  # ok
```
