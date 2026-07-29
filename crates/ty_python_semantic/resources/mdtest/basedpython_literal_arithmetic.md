# basedpython: literal arithmetic for float and complex

ty supports literal arithmetic on `int`s — `1 + 1` is inferred as `Literal[2]`. In basedpython
files, the same goes for `float` and `complex` literals, including mixed numeric operations

## float + float

```by
reveal_type(1.0 + 2.0)  # revealed: 3.0
reveal_type(5.5 - 2.25)  # revealed: 3.25
reveal_type(2.0 * 3.5)  # revealed: 7.0
reveal_type(10.0 / 4.0)  # revealed: 2.5
reveal_type(7.5 // 2.0)  # revealed: 3.0
reveal_type(7.5 % 2.0)  # revealed: 1.5
reveal_type(2.0 ** 3.0)  # revealed: 8.0
```

## float precision is preserved

ieee 754 semantics — `0.1 + 0.2` is exactly what python computes, not `0.3`

```by
reveal_type(0.1 + 0.2)  # revealed: 0.30000000000000004
```

## int promoted to float

```by
reveal_type(1 + 2.0)  # revealed: 3.0
reveal_type(2.0 + 1)  # revealed: 3.0
reveal_type(10 / 4)  # revealed: 2.5
reveal_type(10.0 / 4)  # revealed: 2.5
reveal_type(7 // 2.5)  # revealed: 2.0
```

## int / int produces a float literal

`/` is true division; the result is exact when representable in `f64`

```by
reveal_type(2 / 2)  # revealed: 1.0
reveal_type(7 / 2)  # revealed: 3.5
reveal_type(1 / 3)  # revealed: 0.3333333333333333
```

## bool promoted through float

```by
reveal_type(True + 1.5)  # revealed: 2.5
reveal_type(2.5 + False)  # revealed: 2.5
```

## complex + complex

```by
reveal_type(1j + 2j)  # revealed: 3j
reveal_type(3j - 1j)  # revealed: 2j
reveal_type(2j * 3j)  # revealed: (-6+0j)
reveal_type(4j / 2j)  # revealed: (2+0j)
```

## mixed complex with int and float

```by
reveal_type(1 + 2j)  # revealed: (1+2j)
reveal_type(2j + 1)  # revealed: (1+2j)
reveal_type(1.5 + 2j)  # revealed: (1.5+2j)
reveal_type(2j + 1.5)  # revealed: (1.5+2j)
reveal_type((1 + 2j) * (3 + 4j))  # revealed: (-5+10j)
reveal_type((1 + 2j) - (1 + 1j))  # revealed: 1j
reveal_type((4 + 2j) / (1 + 1j))  # revealed: (3+-1j)
```

## complex floor-div, mod, and pow fall through to dunder dispatch

python forbids `//` and `%` on complex; pow on complex is not modelled as a literal here, so it
widens to the nominal `complex` instance via typeshed's `__pow__`

```by
# error: [unsupported-operator]
reveal_type(1j // 2j)  # revealed: Unknown
# error: [unsupported-operator]
reveal_type(1j % 2j)  # revealed: Unknown
reveal_type(2j ** 2)  # revealed: complex
```

## division by zero falls back to the instance

floats: `1.0 / 0.0` raises at runtime, so no literal `inf` is produced

```by
# error: [division-by-zero]
reveal_type(1.0 / 0)  # revealed: int | float
```

## non-finite pow falls back

`(-1.0) ** 0.5` is a complex at runtime, not an f64 NaN — fall through to dunder dispatch

```by
reveal_type((-1.0) ** 0.5)  # revealed: int | float
```
