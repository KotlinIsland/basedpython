"""the helpers basedpython's emitted python calls at run time

a build writes this file beside the modules it emits, and each module imports
the names it calls. a transpile with nowhere to write it pastes the definitions
it needs in instead. `runtime.rs` slices them out by name, and nothing here is
imported by hand
"""


# --- runtime type-soundness checks ---------------------------------------
# inserted where ty accepts a value on an annotation-level claim it cannot
# verify: a generic call's result, a projection out of a specialized container,
# a loop element, an annotated assignment, a return, an argument


def _soundness_check(_v, _t):
    if not isinstance(_v, _t):
        raise TypeError(
            f"type soundness violation: expected {getattr(_t, '__name__', _t)}, "
            f"got {type(_v).__name__}"
        )
    return _v


def _soundness_iter(_it, _t):
    for _x in _it:
        yield _soundness_check(_x, _t)


async def _soundness_aiter(_it, _t):
    async for _x in _it:
        yield _soundness_check(_x, _t)


# --- checked casts ---------------------------------------------------------
# `value cast! T` verifies at run time and raises on a mismatch; `value cast? T`
# yields None instead. the predicate forms serve any target the parametric
# engine below can decide — a reified-cell comparison (`T == int`), an
# `__orig_class__` probe, a structural protocol check, or a disjunction of those
# across a union's arms. their predicate is a lambda, so the value is evaluated
# exactly once (as `_v`) and referenced from inside the test


def _checked_cast(_v, _t):
    if not isinstance(_v, _t):
        raise TypeError(
            f"cast to {getattr(_t, '__name__', _t)} failed: value is {type(_v).__name__}"
        )
    return _v


def _try_cast(_v, _t):
    return _v if isinstance(_v, _t) else None


def _checked_cast_pred(_v, _pred):
    if not _pred(_v):
        raise TypeError(f"cast failed: value is {type(_v).__name__}")
    return _v


def _try_cast_pred(_v, _pred):
    return _v if _pred(_v) else None


# --- optional values -------------------------------------------------------
# `T?` is a type, not a wrapper, so this class exists for the few places the
# emitted code needs one at run time: `Some(x)`, a force-unwrap, a spelled-out
# cast target


class Optional:
    def __init__(self, value):
        self.value = value

    def __class_getitem__(cls, item):
        return cls

    def __repr__(self):
        return f"Some({self.value!r})"


def _force_unwrap(_v):
    if isinstance(_v, Optional):
        return _v.value
    if _v is None:
        raise RuntimeError("force-unwrap of absent value")
    if isinstance(_v, BaseException):
        raise RuntimeError("force-unwrap of absent value") from _v
    return _v


# --- effects, entry points and loop capture --------------------------------


def _by_raises(_allowed, _name):
    import functools
    import inspect

    def _check(_exc):
        if not isinstance(_exc, _allowed):
            raise AssertionError(
                f"{_name} raised {type(_exc).__name__}, which its `raises` clause does not include"
            ) from _exc

    def _decorate(_fn):
        if inspect.isasyncgenfunction(_fn):
            @functools.wraps(_fn)
            async def _wrapper(*_args, **_kwargs):
                try:
                    async for _item in _fn(*_args, **_kwargs):
                        yield _item
                except BaseException as _exc:
                    _check(_exc)
                    raise
        elif inspect.iscoroutinefunction(_fn):
            @functools.wraps(_fn)
            async def _wrapper(*_args, **_kwargs):
                try:
                    return await _fn(*_args, **_kwargs)
                except BaseException as _exc:
                    _check(_exc)
                    raise
        elif inspect.isgeneratorfunction(_fn):
            @functools.wraps(_fn)
            def _wrapper(*_args, **_kwargs):
                try:
                    yield from _fn(*_args, **_kwargs)
                except BaseException as _exc:
                    _check(_exc)
                    raise
        else:
            @functools.wraps(_fn)
            def _wrapper(*_args, **_kwargs):
                try:
                    return _fn(*_args, **_kwargs)
                except BaseException as _exc:
                    _check(_exc)
                    raise
        return _wrapper

    return _decorate


def _by_main_args(_fn, _params, _extra=None):
    import argparse

    _parser = argparse.ArgumentParser(description=_fn.__doc__)
    for _i, (_name, _type, _kind, _required, _choices) in enumerate(_params):
        _flags = [f"--{_name.replace('_', '-')}"]
        if "_" in _name:
            _flags.append(f"--{_name}")
        if _type is None:
            _parser.add_argument(*_flags, dest=f"o{_i}", action="store_true", default=None)
            _parser.add_argument(
                *[f"--no-{_flag[2:]}" for _flag in _flags],
                dest=f"o{_i}",
                action="store_false",
                default=None,
            )
            continue
        if _kind != "keyword":
            _parser.add_argument(
                f"p{_i}",
                metavar=_name,
                nargs="?",
                type=_type,
                default=None,
                choices=_choices,
            )
        _parser.add_argument(
            *_flags,
            dest=f"o{_i}",
            metavar=_name.upper(),
            type=_type,
            default=None,
            choices=_choices,
        )
    if _extra is None:
        _parsed = vars(_parser.parse_args())
        _rest = []
    else:
        _namespace, _rest = _parser.parse_known_args()
        _parsed = vars(_namespace)
        # `parse_known_args` hands back what it did not recognise as it was
        # written, so the vararg's own annotation is what converts it
        _rest = [_extra(_value) for _value in _rest]
    _args = []
    _kwargs = {}
    _omitted = None
    for _i, (_name, _type, _kind, _required, _choices) in enumerate(_params):
        _value = _parsed.get(f"o{_i}")
        _positional = _parsed.get(f"p{_i}")
        if _value is not None and _positional is not None:
            _parser.error(f"argument {_name}: given both positionally and as an option")
        if _value is None:
            _value = _positional
        if _value is None:
            if _required:
                _parser.error(f"the following arguments are required: {_name}")
            if _kind == "positional":
                _omitted = _name
            continue
        if _kind == "positional":
            if _omitted is not None:
                _parser.error(f"argument {_name}: cannot be given without {_omitted}")
            _args.append(_value)
        else:
            _kwargs[_name] = _value
    for _value in _rest:
        if _omitted is not None:
            _parser.error(f"argument {_value}: cannot be given without {_omitted}")
        _args.append(_value)
    return _args, _kwargs


def _by_loop_bind(**_by_values):
    from types import CellType, FunctionType

    def _by_rebind(_by_fn):
        _by_code = _by_fn.__code__
        _by_bound = FunctionType(
            _by_code,
            _by_fn.__globals__,
            _by_fn.__name__,
            _by_fn.__defaults__,
            tuple(
                CellType(_by_values[_by_name]) if _by_name in _by_values else _by_cell
                for _by_name, _by_cell in zip(_by_code.co_freevars, _by_fn.__closure__ or ())
            ),
        )
        _by_bound.__kwdefaults__ = _by_fn.__kwdefaults__
        _by_bound.__qualname__ = _by_fn.__qualname__
        _by_bound.__doc__ = _by_fn.__doc__
        _by_bound.__dict__.update(_by_fn.__dict__)
        if hasattr(_by_fn, "__annotate__"):
            _by_bound.__annotate__ = _by_fn.__annotate__
        else:
            _by_bound.__annotations__ = _by_fn.__annotations__
        if hasattr(_by_fn, "__type_params__"):
            _by_bound.__type_params__ = _by_fn.__type_params__
        return _by_bound
    return _by_rebind


# --- string templates and graphemes ----------------------------------------


# PEP 750 `Template` / `Interpolation` polyfill for runtimes before 3.14
#
# matches the `string.templatelib` shape a tag relies on: `Template.strings`
# is the literal segments (always one more than the interpolations),
# `Template.interpolations` is the replacement fields, and `Template.values`
# is their evaluated values. iterating a `Template` yields the segments and
# interpolations interleaved in source order, the same as the stdlib type
class _Interpolation:
    def __init__(self, value, expression, conversion=None, format_spec=""):
        self.value = value
        self.expression = expression
        self.conversion = conversion
        self.format_spec = format_spec


class _Template:
    def __init__(self, *args):
        strings = []
        interpolations = []
        if not args or isinstance(args[-1], _Interpolation):
            args = (*args, "")
        pending = ""
        for arg in args:
            if isinstance(arg, _Interpolation):
                strings.append(pending)
                pending = ""
                interpolations.append(arg)
            else:
                pending += arg
        strings.append(pending)
        self.strings = tuple(strings)
        self.interpolations = tuple(interpolations)

    @property
    def values(self):
        return tuple(i.value for i in self.interpolations)

    def __iter__(self):
        for index, string in enumerate(self.strings):
            if string:
                yield string
            if index < len(self.interpolations):
                yield self.interpolations[index]


def _by_graphemes(_text):
    try:
        import regex as _regex
    except ImportError as _err:
        raise ImportError(
            "basedpython's grapheme string surface (character_count / first / last / "
            "characters / character_at / ...) needs the 'regex' package: uv add regex"
        ) from _err
    return _regex.findall(r"\X", _text)


def _by_prefix(_text, _n):
    return "".join(_by_graphemes(_text)[:max(0, _n)])


def _by_suffix(_text, _n):
    _g = _by_graphemes(_text)
    return "".join(_g[max(0, len(_g) - _n):])


# --- class-level properties and reified generics ---------------------------


# binds supplied type arguments onto a type-parameter list, shared by the
# function wrapper and the class specializer
#
# a `TypeVarTuple` takes, as a tuple, the whole run of positional arguments
# the fixed parameters around it don't claim, so `[int, str, bool]` on
# `[T, *Args]` binds `T = int` and `Args = (str, bool)`; a keyword-variadic
# `**Kwargs` sits outside the positional slots entirely and binds the mapping
# of the keyword fields (`f[foo=int]` → `Kwargs = {'foo': int}`, spelled
# `f.__getitem__(foo=int)` in the lowered python, since subscripts take no
# keywords). an omitted slot is filled from its pep 696 default, read off the
# parameter list itself; an unfilled `TypeVarTuple` or `**Kwargs` binds empty,
# and any other slot is simply left out for the caller to answer for.
# over-specializing a parameter list with no variadic raises
def _bind_type_params(params, supplied, fields, owner):
    from typing import ParamSpec, TypeVarTuple
    pack = next((p for p in params if isinstance(p, ParamSpec)), None)
    if pack is None and fields:
        raise TypeError(
            f"{owner} has no keyword-variadic type parameter for "
            f"{', '.join(fields)}"
        )
    slots = [p for p in params if p is not pack]
    variadic = next(
        (i for i, p in enumerate(slots) if isinstance(p, TypeVarTuple)), None
    )
    if variadic is None:
        if len(supplied) > len(slots):
            raise TypeError(
                f"too many type arguments for {owner}: "
                f"expected {len(slots)}, got {len(supplied)}"
            )
        bound = dict(zip((p.__name__ for p in slots), supplied))
    else:
        trailing = slots[variadic + 1:]
        packed = tuple(supplied[variadic:len(supplied) - len(trailing)])
        bound = dict(zip((p.__name__ for p in slots[:variadic]), supplied))
        if packed:
            bound[slots[variadic].__name__] = packed
        bound.update(
            zip(
                (p.__name__ for p in trailing),
                supplied[variadic + len(packed):],
            )
        )
    if fields:
        bound[pack.__name__] = dict(fields)
    for param in params:
        name = param.__name__
        if name in bound:
            continue
        has_default = getattr(param, "has_default", None)
        if has_default is not None and has_default():
            bound[name] = param.__default__
        elif isinstance(param, TypeVarTuple):
            bound[name] = ()
        elif param is pack:
            bound[name] = {}
    return bound



class _by_static_property:
    def __init__(self, fget):
        self._fget = fget
    def __get__(self, instance, owner=None):
        return self._fget(owner if owner is not None else type(instance))


# the `generic` wrapper, for a function that reifies its type parameters
#
# `f[int]` produces a specialized `generic` carrying `args=(int,)`; calling it
# rebuilds the function with a closure whose type-parameter cells hold the
# type arguments, keyed by `co_freevars` name so unrelated cells (captured
# locals, `__class__`) survive. parameter defaults, kwonly defaults and the
# qualname carry over to the rebuilt function
#
# the supplied arguments are mapped onto the parameters by
# `_bind_type_params`, so `f()` works when every reified parameter
# carries a pep 696 default; a slot that binding leaves empty and the body
# reads raises `TypeError` at the call. the wrapper is also a descriptor:
# `__get__` captures the receiver so a reified *method* (`obj.m[int]()`) binds
# `self` like an ordinary method. attribute access falls through to the
# wrapped function, keeping introspection (`f.__name__`, `f.__doc__`) working
class generic:
    def __init__(self, fn, args=None, instance=None, fields=None):
        self.fn = fn
        self.args = args
        self.instance = instance
        self.fields = fields

    def __repr__(self):
        return f"<generic {self.fn!r}>"

    def __getattr__(self, name):
        if name == "fn":
            raise AttributeError(name)
        return getattr(self.fn, name)

    def __get__(self, obj, objtype=None):
        if obj is None:
            return self
        return generic(self.fn, self.args, obj, self.fields)

    def __getitem__(self, *items, **fields):
        if self.args is not None or self.fields is not None:
            raise TypeError("type arguments already specified")
        if len(items) == 1 and isinstance(items[0], tuple):
            items = items[0]
        # reject a bad arity here, not at the call
        _bind_type_params(self.fn.__type_params__, items, fields, self.fn.__name__)
        return generic(self.fn, items, self.instance, fields)

    def __call__(self, *args, **kwargs):
        from types import CellType, FunctionType

        fn = self.fn
        code = fn.__code__
        values = _bind_type_params(
            fn.__type_params__, self.args or (), self.fields or {}, fn.__name__
        )
        for param in fn.__type_params__:
            name = param.__name__
            if name not in values and name in code.co_freevars:
                # a synthesized parameter stands for an erased union the user
                # never spelled, so naming it would leak the lowering
                if name.startswith("__by_erased"):
                    raise TypeError(
                        f"{fn.__name__}() cannot tell which specialization it was "
                        f"given: the argument's type arguments are erased at "
                        f"runtime, and the call site did not record them"
                    )
                raise TypeError(f"{fn.__name__}() missing a type argument for {name!r}")
        closure = tuple(
            CellType(values[name]) if name in values else cell
            for name, cell in zip(code.co_freevars, fn.__closure__ or ())
        )
        temp_fn = FunctionType(code, fn.__globals__, fn.__name__, fn.__defaults__, closure)
        temp_fn.__kwdefaults__ = fn.__kwdefaults__
        temp_fn.__qualname__ = fn.__qualname__
        if self.instance is not None:
            return temp_fn(self.instance, *args, **kwargs)
        return temp_fn(*args, **kwargs)


# --- conformance registry --------------------------------------------------
# a module declaring `extension str(Show)` registers the conformance when it is
# imported, and every module that tests one reads the same registry


# the runtime a conformance needs: the registry, the per-member lookup, the
# `is`-test, and the two dispatchers (a method is fetched and called by the
# parentheses that already follow the access; a data member is read)
#
# three things here are load-bearing and were each a bug before:
#
# - **the registry is per *process*, not per module.** a module-level `{}` would
#   be private to whichever copy of these helpers ran — and a transpile with
#   nowhere to write this file pastes them into each module that needs them, so
#   there can be several. it is parked in `sys.modules` instead, which is the one
#   namespace every module already shares
# - **the lookup is per *member*.** walking the MRO for the first class with
#   *any* table would let a base's conformance beat a subclass's own method —
#   the same object answering two ways depending on its static type. whichever
#   comes first in the MRO wins: a table entry for this member, or a class that
#   defines it
# - **a conformance registers under every interface it implies.** conforming to
#   `Loud(Show)` conforms to `Show`, and a receiver typed as `Show` looks up
#   under `Show`
def _by_registry():
    # one registry per process: a module that pasted these helpers in has a copy
    # of its own, and a conformance registered through any copy has to be
    # visible to all of them. `sys.modules` is the namespace they already share.
    # imported inside the function so the lazy-import pass has no statement to
    # rewrite
    import sys
    import types
    module = sys.modules.get("_by_conformance_registry")
    if module is None:
        module = types.ModuleType("_by_conformance_registry")
        module.table = {}
        sys.modules["_by_conformance_registry"] = module
    return module.table

_by_conformances = _by_registry()

def _by_conform(interface, cls, witness):
    # conforming to an interface conforms to everything it derives, so a
    # receiver typed as a supertype finds the same witness
    for base in getattr(interface, "__mro__", (interface,)):
        if base is object or getattr(base, "__module__", None) == "typing":
            continue
        _by_conformances.setdefault(base, {}).setdefault(cls, {}).update(witness)

def _by_witness_entry(value, interface, name):
    table = _by_conformances.get(interface)
    if table is None:
        return None
    for cls in type(value).__mro__:
        witness = table.get(cls)
        if witness is not None and name in witness:
            return witness[name]
        # a class that defines the member itself answers it, and beats any
        # conformance registered further up the mro
        if name in cls.__dict__:
            return None
    return None

def _by_conforms(value, interface, members=None):
    table = _by_conformances.get(interface)
    if table is not None:
        for cls in type(value).__mro__:
            if cls in table:
                return True
    if members is None:
        return isinstance(value, interface)
    return all(hasattr(value, name) for name in members)

def _by_witness(value, interface, name):
    function = _by_witness_entry(value, interface, name)
    if function is None:
        return getattr(value, name)
    return lambda *args, **kwargs: function(value, *args, **kwargs)

def _by_witness_class(value, interface, name):
    function = _by_witness_entry(value, interface, name)
    if function is None:
        return getattr(value, name)
    owner = value if isinstance(value, type) else type(value)
    return lambda *args, **kwargs: function(owner, *args, **kwargs)

def _by_witness_get(value, interface, name):
    function = _by_witness_entry(value, interface, name)
    if function is None:
        return getattr(value, name)
    return function(value)


# --- parametric type tests -------------------------------------------------
# the one engine behind `is`, `cast` and the deep soundness checks: given a
# value and an alias, decide whether the value really is that specialization


def _by_type_param_defaults(args):
    # a class records its generic bases *unsubstituted* — `class L[T = Never]
    # (list[T])` stores `list[T]`, never `list[Never]` — so a type parameter
    # left at its pep 696 default resolves to that default rather than staying a
    # bare TypeVar that matches nothing
    resolved = []
    substituted = False
    for arg in args:
        has_default = getattr(arg, "has_default", None)
        if has_default is not None and has_default():
            resolved.append(arg.__default__)
            substituted = True
        else:
            resolved.append(arg)
    return tuple(resolved) if substituted else args

def _by_alias(value):
    # a reified generic class specializes to a *subclass*, which records the
    # alias it stands for; anything else already is what it says it is. read
    # from the class's own dict, so an ordinary subclass of a specialization is
    # not mistaken for one
    if isinstance(value, type):
        return value.__dict__.get("__orig_class__", value)
    return value

def _by_subst(annotation, mapping):
    # replace type parameters with the arguments bound to them, rebuilding
    # nested aliases (`list[dict[str, T]]` with `T = int` → `list[dict[str, int]]`)
    annotation = _by_alias(annotation)
    try:
        if annotation in mapping:
            return mapping[annotation]
    except TypeError:
        pass
    args = getattr(annotation, "__args__", ())
    if not args:
        return annotation
    replaced = tuple(_by_subst(arg, mapping) for arg in args)
    if replaced == args:
        return annotation
    origin = getattr(annotation, "__origin__", None)
    if origin is None:
        return annotation
    try:
        return origin[replaced]
    except TypeError:
        return annotation

def _by_specialize(alias, origin, depth=0):
    # the arguments with which `alias` satisfies `origin`, resolved *down the
    # declared base chain* rather than assumed to line up positionally. a base
    # that fixes or reorders its arguments is then followed faithfully:
    # `class Odd[T](list[int])` is a `list[int]` whatever `T` is, and
    # `class Swap[A, B](dict[B, A])` specializes `dict` in the other order
    if depth > 16:
        return None
    alias = _by_alias(alias)
    klass = getattr(alias, "__origin__", alias)
    if not isinstance(klass, type):
        return None
    args = getattr(alias, "__args__", ())
    params = getattr(klass, "__type_params__", ())
    if not args:
        defaulted = _by_type_param_defaults(params)
        if defaulted is not params:
            args = defaulted
    if klass is origin:
        return args or None
    mapping = {}
    for param, arg in zip(params, args):
        try:
            mapping[param] = arg
        except TypeError:
            pass
    bases = klass.__dict__.get("__orig_bases__")
    if bases is None:
        # a class inheriting only plain classes records no `__orig_bases__`
        bases = getattr(klass, "__bases__", ())
    for base in bases:
        found = _by_specialize(_by_subst(base, mapping) if mapping else base, origin, depth + 1)
        if found is not None:
            return found
    # the declared bases don't reach `origin`: a builtin registered as a *virtual*
    # subclass of an abc (`list` for `Sequence`) has no base to walk. its
    # arguments do line up positionally once membership is established. this runs
    # only after resolution has failed, so it applies to the already-resolved base
    # (`list[int]`), never to a subclass that fixes or reorders arguments
    if args and isinstance(origin, type):
        try:
            if issubclass(klass, origin):
                return args
        except TypeError:
            pass
    return None

def _by_generic_args(value, origin):
    # an explicit `A[int]()` records its specialization on the instance;
    # otherwise the class itself is the starting point and any pep 696 defaults
    # stand in for the arguments it was constructed with
    reified = getattr(value, "__orig_class__", None)
    found = _by_specialize(reified if reified is not None else type(value), origin)
    return [found] if found is not None else []

def _parametric_is(value, alias, variances):
    alias = _by_alias(getattr(alias, "__value__", alias))
    origin = getattr(alias, "__origin__", alias)
    if not isinstance(value, origin):
        return False
    target_args = getattr(alias, "__args__", ())
    if len(target_args) != len(variances):
        return False
    for reified_args in _by_generic_args(value, origin):
        if len(reified_args) != len(target_args):
            continue
        for r, t, v in zip(reified_args, target_args, variances):
            if v == 3 or r == t:
                continue
            if v == 1 and _parametric_is_sub(r, t):
                continue
            if v == 2 and _parametric_is_sub(t, r):
                continue
            break
        else:
            return True
    return False

def _parametric_is_sub(a, b):
    if a is b or b is object:
        return True
    a_origin = getattr(a, "__origin__", a)
    b_origin = getattr(b, "__origin__", b)
    if isinstance(a_origin, type) and isinstance(b_origin, type) and not getattr(b, "__args__", ()):
        try:
            return issubclass(a_origin, b_origin)
        except TypeError:
            return False
    return a == b

def _parametric_is_lenient(value, alias, variances):
    # the checked-cast form: a value that records no reification has no
    # arguments to check, so the base class test is the whole guarantee. this is
    # what keeps `[1, 2] cast list[int]` legal while still rejecting a value
    # whose recorded arguments contradict the target
    alias = _by_alias(getattr(alias, "__value__", alias))
    origin = getattr(alias, "__origin__", alias)
    if not isinstance(value, origin):
        return False
    if not _by_generic_args(value, origin):
        return True
    return _parametric_is(value, alias, variances)


# --- structural protocol tests ---------------------------------------------


_by_proto_missing = object()

def _by_member_annotation(klass, name):
    try:
        import typing
        hints = typing.get_type_hints(klass)
    except Exception:
        hints = None
    if hints is not None and name in hints:
        return hints[name]
    for base in klass.__mro__:
        annotations = base.__dict__.get("__annotations__", {})
        if name in annotations:
            return annotations[name]
    return _by_proto_missing

def _by_lit(*values):
    # rebuild `typing.Literal[…]` for a literal type argument (`A[True]`
    # specializes `T` to `Literal[True]`). spelled as a call so the member list
    # needs no import of its own — this helper ships with the check
    import typing
    return typing.Literal[values]

def _by_literal_args(t):
    import typing
    return typing.get_args(t) if typing.get_origin(t) is typing.Literal else None

def _by_proto_sub(a, b):
    if a is b or b is object:
        return True
    a_values = _by_literal_args(a)
    b_values = _by_literal_args(b)
    if a_values is not None:
        # `Literal[True]` is a subtype of another literal that lists all its
        # values, and of any class every value is an instance of
        if b_values is not None:
            return all(value in b_values for value in a_values)
        return isinstance(b, type) and all(isinstance(value, b) for value in a_values)
    if b_values is not None:
        # a whole class is never a subtype of a narrower literal
        return False
    a_origin = getattr(a, "__origin__", a)
    b_origin = getattr(b, "__origin__", b)
    if isinstance(a_origin, type) and isinstance(b_origin, type) and not getattr(b, "__args__", ()):
        try:
            return issubclass(a_origin, b_origin)
        except TypeError:
            return False
    return a == b

def _by_variance_ok(actual, expected, variance):
    # 0 invariant (equality), 1 covariant (actual <: expected),
    # 2 contravariant (expected <: actual), 3 bivariant (any)
    if variance == 3 or actual == expected:
        return True
    if variance == 1 and _by_proto_sub(actual, expected):
        return True
    if variance == 2 and _by_proto_sub(expected, actual):
        return True
    return False

def _by_method_matches(klass, name, params, ret):
    method = getattr(klass, name, None)
    if not callable(method):
        return False
    import inspect, typing
    try:
        signature = inspect.signature(method)
        hints = typing.get_type_hints(method)
    except Exception:
        return False
    positional = [
        p for p in signature.parameters.values()
        if p.kind in (inspect.Parameter.POSITIONAL_ONLY, inspect.Parameter.POSITIONAL_OR_KEYWORD)
    ]
    # drop the receiver (`self` / `cls`) an unbound method still carries
    positional = positional[1:]
    if len(positional) < len(params):
        return False
    # extra positional parameters the protocol doesn't supply must be optional,
    # else a caller matching the protocol would fail to provide them
    for p in positional[len(params):]:
        if p.default is inspect.Parameter.empty:
            return False
    # likewise any required keyword-only parameter would break a protocol call
    for p in signature.parameters.values():
        if p.kind == inspect.Parameter.KEYWORD_ONLY and p.default is inspect.Parameter.empty:
            return False
    for (expected, variance), p in zip(params, positional):
        if p.name in hints:
            actual = hints[p.name]
        elif p.default is not inspect.Parameter.empty:
            # a reified default gives the parameter's inferred type at runtime
            actual = type(p.default)
        else:
            return False
        if not _by_variance_ok(actual, expected, variance):
            return False
    if ret is not None:
        expected, variance = ret
        if "return" not in hints or not _by_variance_ok(hints["return"], expected, variance):
            return False
    return True

# a parametric test against a *protocol* target
# (`value is A[int]`). a protocol's instances never record which
# specialization they satisfy, so `__orig_class__` can't answer it — but
# basedpython reifies annotations, so the value's class is checked
# structurally: each protocol member's reified annotation must match the
# member's specialized type. `members` is a list of kind-tagged tuples:
#
# - `("attr", name, expected_type, variance)` — a data member, checked against
#   the value class's annotation for `name`
# - `("method", name, [(type, variance), …], return_or_None)` — a method
#   member, whose parameters (contravariant) and return (covariant) are checked
#   against the value method's reified parameter/return annotations; a
#   parameter with no annotation but a default falls back to `type(default)`
#
# `variance` is a code the transpiler picks per argument (0 invariant → equality, 1
# covariant → subtype, 2 contravariant → supertype, 3 bivariant → any).
# annotations are read with `typing.get_type_hints` (resolving string
# annotations and inherited members), falling back to a raw `__mro__` walk
def _by_protocol_is(value, members):
    klass = type(value)
    for member in members:
        kind = member[0]
        if kind == "attr":
            _, name, expected, variance = member
            actual = _by_member_annotation(klass, name)
            if actual is _by_proto_missing:
                # the member is *there*, it just carries no annotation any
                # runtime can read — python records nothing for a `self.a: int`
                # written inside `__init__`. answering `False` would contradict
                # the checker, which accepts that class as satisfying the
                # protocol, so refuse to answer rather than answer wrongly
                if hasattr(value, name):
                    raise TypeError(
                        "cannot check `" + klass.__qualname__ + "." + name
                        + "` against a parameterized protocol: its type is declared "
                        + "inside a method, and only a class-level annotation "
                        + "survives to runtime. declare it in the class body"
                    )
                return False
            if not _by_variance_ok(actual, expected, variance):
                return False
        else:
            _, name, params, ret = member
            if not _by_method_matches(klass, name, params, ret):
                return False
    return True


# --- template literal types ------------------------------------------------


import re as _by_re

# matches a value against a template literal type — a pattern such as
# `f"a{int}b"`, whose type is the set of strings it can produce
#
# the regular expression comes from the checker, which builds it from the same
# reading of the pattern's holes that decides the static answer, so the test
# accepts exactly the strings the type contains. a non-`str` value is not one
# of them
def _by_pattern_is(value, pattern):
    return isinstance(value, str) and _by_re.fullmatch(pattern, value) is not None


# --- deep soundness checks -------------------------------------------------
# a specialized target validates its base class always, and its reified type
# arguments when the value carries them (`__orig_class__`, stamped by `A[int](…)`).
# a value with no reification passes the argument check — its parameters are not
# available to check, leaving the base `isinstance` as the guarantee


def _soundness_parametric(_v, _alias, _variances):
    _alias = _by_alias(_alias)
    _origin = getattr(_alias, "__origin__", _alias)
    if not isinstance(_v, _origin):
        raise TypeError(
            f"type soundness violation: expected {getattr(_origin, '__name__', _origin)}, "
            f"got {type(_v).__name__}"
        )
    if getattr(_v, "__orig_class__", None) is not None and not _parametric_is(_v, _alias, _variances):
        raise TypeError(
            f"type soundness violation: expected {_alias}, got {_v.__orig_class__}"
        )
    return _v


def _soundness_iter_p(_it, _alias, _variances):
    for _x in _it:
        yield _soundness_parametric(_x, _alias, _variances)


async def _soundness_aiter_p(_it, _alias, _variances):
    async for _x in _it:
        yield _soundness_parametric(_x, _alias, _variances)


# --- reified class generics ------------------------------------------------
# `A[int]` is a memoized subclass that records its arguments, so an instance
# can be asked what it was specialized to


_by_absent = object()


# the `generic_class` decorator, for a class that reifies its type parameters
#
# it replaces the class's `__class_getitem__`, so `A[int]` no longer builds a
# `typing` alias but a memoized subclass of `A` carrying the type arguments —
# which is what makes them readable from `__new__` and `__init__` onwards,
# where an `__orig_class__` stamp applied after construction is not yet there.
# being a real subclass also keeps `isinstance(a, A)` and `class B(A[int])`
# working, neither of which survives an alias standing in for a class; the
# specialization declares an empty `__slots__` so a slotted class stays slotted,
# and `__init_subclass__` is held back for it, since it is the same class with
# its arguments fixed rather than a subclass the program wrote
#
# each specialization composes what it binds with what its bases already bound
# and resolves the chain, so `class B[U](A[U])` specialized as `B[int]` answers
# `T` with `int` and not with `U`. `__orig_class__` is carried as a class
# attribute, which is where the alias would have put it, so every reader of a
# runtime specialization — `_parametric_is` included — sees the same thing it
# saw before
#
# `_type_argument` answers one read. it takes the receiver rather than the
# class so a `classmethod` can pass `cls` and everything else `self`, and it
# raises rather than returning the `TypeVar` object the parameter would
# otherwise still name — whether because nothing specialized the class or
# because a base's argument was never filled in
def generic_class(cls):
    cls.__class_getitem__ = classmethod(_specialize)
    return cls


def _specialize(cls, item):
    from types import GenericAlias
    from typing import TypeVar, TypeVarTuple

    args = item if isinstance(item, tuple) else (item,)
    if "__by_type_arguments__" in cls.__dict__:
        raise TypeError(f"{cls.__name__} is already specialized")
    cache = cls.__dict__.get("__by_specializations__")
    if cache is None:
        cache = {}
        cls.__by_specializations__ = cache
    try:
        made = cache.get(args)
    except TypeError:
        raise TypeError(
            f"a type argument to {cls.__name__} is not hashable, so the "
            f"specialization it names cannot be built"
        ) from None
    if made is not None:
        return made
    params = cls.__type_params__
    bound = {}
    for base in reversed(cls.__mro__):
        bound.update(base.__dict__.get("__by_type_arguments__") or {})
    bound.update(_bind_type_params(params, args, {}, cls.__name__))
    for param in params:
        if param.__name__ not in bound:
            raise TypeError(
                f"too few type arguments for {cls.__name__}: "
                f"no argument for {param.__name__!r}"
            )
    for name, value in bound.items():
        seen = {name}
        while isinstance(value, (TypeVar, TypeVarTuple)) and value.__name__ in bound:
            if value.__name__ in seen:
                break
            seen.add(value.__name__)
            value = bound[value.__name__]
        bound[name] = value
    namespace = {
        "__by_type_arguments__": bound,
        "__orig_class__": GenericAlias(cls, args),
        # the specialization declares nothing of its own, so a slotted class
        # stays slotted instead of gaining a `__dict__` here
        "__slots__": (),
    }
    # a specialization is the same class with its arguments fixed, not a
    # subclass the program wrote, so the hook that greets a subclass must not
    # run for it: it would be handed neither the class keywords the definition
    # was given nor a class anybody declared
    saved = cls.__dict__.get("__init_subclass__", _by_absent)
    cls.__init_subclass__ = classmethod(lambda cls, **kwargs: None)
    try:
        made = type(cls)(cls.__name__, (cls,), namespace)
    except TypeError as exc:
        # a metaclass that takes class-creation keywords cannot be given them
        # again: nothing records what the definition was written with
        raise TypeError(
            f"cannot build a specialization of {cls.__name__}: {exc}"
        ) from exc
    finally:
        if saved is _by_absent:
            del cls.__init_subclass__
        else:
            cls.__init_subclass__ = saved
    made.__module__ = cls.__module__
    made.__qualname__ = cls.__qualname__
    cache[args] = made
    return made


def _type_argument(owner, name):
    from typing import TypeVar, TypeVarTuple

    cls = owner if isinstance(owner, type) else type(owner)
    bound = getattr(cls, "__by_type_arguments__", None)
    value = _by_absent if bound is None else bound.get(name, _by_absent)
    # a value still standing as a type parameter is a base's argument that
    # nothing filled in, which means the instance came from the bare class
    if value is _by_absent or isinstance(value, (TypeVar, TypeVarTuple)):
        raise TypeError(
            f"{cls.__name__} has no type argument for {name!r}: it was not "
            f"constructed from a specialization"
        )
    return value


# --- lazy imports ----------------------------------------------------------
# below python 3.15 there is no `lazy` keyword, so an import is rewritten to a
# call: a module import becomes `_lazy_module`, and a `from` import a proxy that
# resolves the attribute the first time anything touches it
#
# `__class__` makes `isinstance(proxy, C)` work, and `__instancecheck__` makes
# `isinstance(x, proxy)` work for a lazily-imported class (`isinstance` looks
# `__instancecheck__` up on `type(classinfo)`, which is `_LazyAttr`).
# `type(proxy)` and `proxy is x` cannot be fixed by any proxy — that is exactly
# why PEP 810 is a language feature — and are documented limits of this
# polyfill


import importlib as _by_il, importlib.util as _by_iu, sys as _by_sys
# a relative import names its module against the package doing the importing,
# which only that module knows, so it hands `__package__` in
def _lazy_module(name, package=None):
    if package is not None:
        name = _by_iu.resolve_name(name, package)
    mod = _by_sys.modules.get(name)
    if mod is not None:
        return mod
    if "." in name:
        return _by_il.import_module(name)
    spec = _by_iu.find_spec(name)
    if spec is None or spec.loader is None:
        raise ImportError(f"No module named {name!r}", name=name)
    spec.loader = _by_iu.LazyLoader(spec.loader)
    mod = _by_iu.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return _by_sys.modules.setdefault(name, mod)


def _by_forward_operators(proxy):
    import operator as op
    def one(f): return lambda s: f(s._by_resolve())
    def two(f): return lambda s, o: f(s._by_resolve(), o)
    def rtwo(f): return lambda s, o: f(o, s._by_resolve())
    for n, f in (("add", op.add), ("sub", op.sub), ("mul", op.mul), ("matmul", op.matmul),
                 ("truediv", op.truediv), ("floordiv", op.floordiv), ("mod", op.mod),
                 ("divmod", divmod), ("pow", op.pow), ("lshift", op.lshift),
                 ("rshift", op.rshift), ("and", op.and_), ("xor", op.xor), ("or", op.or_)):
        setattr(proxy, "__" + n + "__", two(f))
        setattr(proxy, "__r" + n + "__", rtwo(f))
    for n in ("lt", "le", "eq", "ne", "gt", "ge"):
        setattr(proxy, "__" + n + "__", two(getattr(op, n)))
    for n, f in (("neg", op.neg), ("pos", op.pos), ("abs", abs), ("invert", op.inv),
                 ("len", len), ("iter", iter), ("next", next), ("bool", bool),
                 ("str", str), ("repr", repr), ("bytes", bytes), ("int", int),
                 ("float", float), ("complex", complex), ("index", op.index),
                 ("hash", hash), ("reversed", reversed)):
        setattr(proxy, "__" + n + "__", one(f))
    setattr(proxy, "__getitem__", two(op.getitem))
    setattr(proxy, "__contains__", two(op.contains))
    setattr(proxy, "__delitem__", two(op.delitem))
    setattr(proxy, "__setitem__", lambda s, k, v: op.setitem(s._by_resolve(), k, v))
    setattr(proxy, "__format__", lambda s, f: format(s._by_resolve(), f))
    setattr(proxy, "__round__", lambda s, *a: round(s._by_resolve(), *a))
    setattr(proxy, "__enter__", lambda s: s._by_resolve().__enter__())
    setattr(proxy, "__exit__", lambda s, *a: s._by_resolve().__exit__(*a))

class _LazyAttr:
    __slots__ = ("_by_mod", "_by_attr", "_by_val", "_by_has")
    def __init__(self, mod, attr):
        object.__setattr__(self, "_by_mod", mod)
        object.__setattr__(self, "_by_attr", attr)
        object.__setattr__(self, "_by_val", None)
        object.__setattr__(self, "_by_has", False)
    def _by_resolve(self):
        if not self._by_has:
            m = _lazy_module(self._by_mod)
            try:
                v = getattr(m, self._by_attr)
            except AttributeError:
                # a submodule rather than an attribute: `urllib/__init__.py` never
                # imports `parse`, and cpython binds it only because `__import__` is
                # handed a fromlist. reading the attribute alone never triggers that
                try:
                    v = _by_il.import_module(self._by_mod + "." + self._by_attr)
                except ImportError:
                    # worded as the import machinery words it, down to the module's
                    # file: a `from x import y` that fails is something programs catch
                    # and report, so the report must not say where the import was
                    # written. `name_from` is left off — cpython's own constructor
                    # only took it from 3.12, and this polyfill runs on 3.9
                    p = getattr(m, "__file__", None)
                    raise ImportError("cannot import name " + repr(self._by_attr) +
                                      " from " + repr(self._by_mod) +
                                      ("" if p is None else " (" + p + ")"),
                                      name=self._by_mod, path=p) from None
            object.__setattr__(self, "_by_val", v)
            object.__setattr__(self, "_by_has", True)
        return self._by_val
    @property
    def __class__(self): return self._by_resolve().__class__
    def __getattr__(self, k): return getattr(self._by_resolve(), k)
    def __setattr__(self, k, v): setattr(self._by_resolve(), k, v)
    def __delattr__(self, k): delattr(self._by_resolve(), k)
    def __call__(self, *a, **k): return self._by_resolve()(*a, **k)
    def __class_getitem__(cls, k): return cls
    def __instancecheck__(self, o): return isinstance(o, self._by_resolve())
    def __subclasscheck__(self, o): return issubclass(o, self._by_resolve())
    def __mro_entries__(self, bases):
        r = self._by_resolve()
        m = getattr(r, "__mro_entries__", None)
        if m is None: return (r,)
        return m(tuple(r if b is self else b for b in bases))

_by_forward_operators(_LazyAttr)

def _lazy_attr(mod, attr, package=None):
    if package is not None:
        mod = _by_iu.resolve_name(mod, package)
    return _LazyAttr(mod, attr)


# a type-only marker for `ty_extensions` names, which have no runtime import
# to make. it supports the type-expression operations the language allows on
# them and nothing else


class _TyExtMarker:
    def __class_getitem__(cls, k): return cls


# `Character` is a concrete `str` subclass, so the grapheme accessors build real
# instances and `isinstance(x, Character)` works. class *identity* is what that
# tests, and a module that pastes the definition in rather than importing it
# would get a class of its own — so the class is interned in a `sys.modules`
# registry and the first definer wins


def _by_character_class():
    import sys
    import types

    registry = sys.modules.setdefault(
        "_by_character_registry", types.ModuleType("_by_character_registry")
    )
    if not hasattr(registry, "Character"):
        class Character(str):
            __slots__ = ()

        registry.Character = Character
    return registry.Character


Character = _by_character_class()


# --- discarded returns -----------------------------------------------------
# the adapter a callable is wrapped in where the site declared one returning
# `None` and the callable returns something else — basedpython's coercion to
# `None`, which the checker resolves as a conversion route
#
# a bare closure would throw the result away just as well. it would also stop
# comparing equal to the callable it wraps, and python deregisters callbacks by
# value all the time — `observers.remove(cb)`, `atexit.unregister(cb)`,
# `signal.disconnect(cb)`. delegating `__eq__` and `__hash__` is what keeps a
# wrapped callback removable; delegating everything else through `__getattr__`
# is what keeps `cb.__name__` answering for a framework that reads it


class _by_discard:
    __slots__ = ("__wrapped__",)

    def __init__(self, fn):
        self.__wrapped__ = fn

    def __call__(self, *args, **kwargs):
        self.__wrapped__(*args, **kwargs)

    def __getattr__(self, name):
        if name == "__wrapped__":
            raise AttributeError(name)
        return getattr(self.__wrapped__, name)

    def __eq__(self, other):
        if isinstance(other, _by_discard):
            other = other.__wrapped__
        return self.__wrapped__ == other

    def __hash__(self):
        return hash(self.__wrapped__)


# --- match statement -------------------------------------------------------
# below 3.10 there is no `match`, so one is lowered to an `if`/`elif` chain and
# these answer the questions the chain cannot ask in an expression
#
# `_by_match_miss` stands for "this pattern did not match" where `None` would be
# ambiguous — a subject really can hold `None`


_by_match_miss = object()

# the sequence types are looked up on first use and kept, rather than imported
# when this module loads: `array` and `collections.abc` are of no interest to a
# program with no sequence pattern in it
_by_match_seq_types = None


# python decides "is a sequence" by a type flag rather than by an ABC, and sets
# it on a handful of builtins that register no ABC of their own. str, bytes and
# bytearray carry the flag's opposite: they are sequences everywhere else, and
# never match a sequence pattern
def _by_match_seq(subject):
    global _by_match_seq_types
    if _by_match_seq_types is None:
        import array
        from collections.abc import Sequence

        _by_match_seq_types = (list, tuple, range, memoryview, array.array, Sequence)
    return isinstance(subject, _by_match_seq_types) and not isinstance(
        subject, (str, bytes, bytearray)
    )


def _by_match_map(subject):
    from collections.abc import Mapping

    return isinstance(subject, Mapping)


def _by_match_key(subject, key):
    try:
        return subject[key]
    except KeyError:
        return _by_match_miss


def _by_match_rest(subject, matched):
    return {key: value for key, value in subject.items() if key not in matched}


# a handful of builtins take one positional sub-pattern that matches the subject
# itself, in place of reading `__match_args__`
_by_match_self = (bool, bytearray, bytes, dict, float, frozenset, int, list, set, str, tuple)


def _by_match_args(cls, subject, count):
    if cls in _by_match_self:
        if count > 1:
            raise TypeError(f"{cls.__name__}() accepts 1 positional sub-pattern ({count} given)")
        return (subject,)
    args = getattr(cls, "__match_args__", ())
    if not isinstance(args, tuple):
        raise TypeError(f"{cls.__name__}.__match_args__ must be a tuple (got {type(args).__name__})")
    if count > len(args):
        raise TypeError(f"{cls.__name__}() accepts {len(args)} positional sub-patterns ({count} given)")
    values = []
    for name in args[:count]:
        if not isinstance(name, str):
            raise TypeError(f"__match_args__ elements must be strings (got {type(name).__name__})")
        try:
            values.append(getattr(subject, name))
        except AttributeError:
            return _by_match_miss
    return tuple(values)


def _by_match_attr(subject, name):
    try:
        return getattr(subject, name)
    except AttributeError:
        return _by_match_miss
