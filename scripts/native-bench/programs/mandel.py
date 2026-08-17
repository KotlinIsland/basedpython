"""scalar float arithmetic with a call in the inner loop

the pair with `mandel_inline` is the measurement: same work, one with the escape
loop behind a call and one with it spelled out, so the difference is what a call
costs on the hot path
"""


def escape(cr: float, ci: float, limit: int) -> int:
    zr = 0.0
    zi = 0.0
    k = 0
    while k < limit:
        if zr * zr + zi * zi > 4.0:
            return k
        t = zr * zr - zi * zi + cr
        zi = 2.0 * zr * zi + ci
        zr = t
        k = k + 1
    return limit


def render(width: int, height: int, limit: int) -> int:
    total = 0
    y = 0
    while y < height:
        x = 0
        while x < width:
            cr = -2.0 + 3.0 * x / width
            ci = -1.2 + 2.4 * y / height
            total = total + escape(cr, ci, limit)
            x = x + 1
        y = y + 1
    return total


def bench() -> int:
    return render(120, 120, 40)
