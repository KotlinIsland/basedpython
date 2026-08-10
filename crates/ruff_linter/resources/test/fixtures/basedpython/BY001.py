# `??` is a parse error in a .py file, so nothing here is rewritable
def f(a, b):
    _ = a if a is not None else b
    _ = b if a is None else a
