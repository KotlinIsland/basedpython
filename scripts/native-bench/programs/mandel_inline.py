"""the same work as `mandel`, with the escape loop inlined

the shape a nested loop takes when nobody factored the inner one out: the outer
body's locals feed the inner arithmetic and the inner loop leaves by `break`.
this once made the type checker not converge at all, so it is a canary as well
as a benchmark
"""


def render(width: int, height: int, limit: int) -> int:
    total = 0
    y = 0
    while y < height:
        x = 0
        while x < width:
            cr = -2.0 + 3.0 * x / width
            ci = -1.2 + 2.4 * y / height
            zr = 0.0
            zi = 0.0
            k = 0
            while k < limit:
                if zr * zr + zi * zi > 4.0:
                    break
                t = zr * zr - zi * zi + cr
                zi = 2.0 * zr * zi + ci
                zr = t
                k = k + 1
            total = total + k
            x = x + 1
        y = y + 1
    return total


def bench() -> int:
    return render(120, 120, 40)
