# `?.` is a parse error in a .py file, so nothing here is rewritable
def f(user):
    _ = None if user is None else user.name
    _ = user.name if user is not None else None
