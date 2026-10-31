"""the helpers basedpython's emitted python calls at run time

a build writes this file beside the modules it emits, and each module imports
the names it calls. a transpile with nowhere to write it pastes the definitions
it needs in instead. `runtime.rs` slices them out by name, and nothing here is
imported by hand
"""


# --- sealed classes -------------------------------------------------------
# a subclass adds itself to the tuple of each sealed class it directly derives
# from, as its class statement runs. only a sealed class has a tuple of its own,
# since a subclass of one inherits the attribute


def _by_sealed_member(_cls):
    # a class statement in a function runs per call, and each run is the same member, so the
    # tuple doesn't grow and the classes of earlier runs can be collected
    _name = (_cls.__module__, _cls.__qualname__)
    for _base in _cls.__bases__:
        if "__sealed_members__" in vars(_base):
            _base.__sealed_members__ = tuple(
                _member
                for _member in _base.__sealed_members__
                if (_member.__module__, _member.__qualname__) != _name
            ) + (_cls,)


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


# a check in a function on a class the module binds further down, or only on some
# paths. every binding the module makes of each of `_names` binds the class the check
# names, so once they are all bound `_target` builds the target the checker named, and
# `_check` is made against it; before then the value passes unchecked. the names are
# looked up in the globals of the module the check is written in, which the lambda
# `_target` carries
def _soundness_once_bound(_names, _v, _check, _target, *_rest):
    _globals = _target.__globals__
    # a deferred `from` import is bound once its statement has run, which its
    # module's `_LazyFrom` knows, and is in the namespace only once something has
    # read it. the `_LazyFrom` is the module's `__getattr__`'s receiver, asked by
    # what it holds rather than by its class: this check is often pasted in by
    # another pass than the import's, and each paste has a class of its own
    _deferred = getattr(getattr(_globals.get("__getattr__"), "__self__", None), "__by_from__", {})
    for _name in _names:
        if _name not in _globals and _deferred.get(_name) is None:
            return _v
    return _check(_v, _target(), *_rest)


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


# --- parameter defaults ----------------------------------------------------
# a default re-evaluated per call has no value until a call evaluates it, so the
# signature holds a marker in its place and the body tests for it. each default has
# a marker of its own, bound at module level under a name of the transpiler's, and
# the body's guard tests for that one object. its `repr` is the default as
# basedpython reads it, which is what `inspect.signature` shows; `<required>` stands
# for a parameter python cannot declare required where it stands. a marker handed
# back as an argument, as `apply_defaults` hands it, is the argument not given
#
# a marker reduces to the name it is bound under, so a pickled one is read back from
# that name, and a copy of one — which `copy` makes through the same reduction — is
# the marker itself. a tool that copies or pickles a signature's defaults hands back
# the object the guard tests for


class _by_default:
    def __init__(self, source, module, name):
        self._by_source = source
        self.__module__ = module
        self._by_name = name

    def __repr__(self):
        if self._by_source is None:
            return "<required>"
        return f"<default: {self._by_source}>"

    def __reduce__(self):
        return self._by_name


# --- effects, entry points and loop capture --------------------------------


# a clause naming a reified type parameter is passed as `_resolve`, a lambda
# building the test from the parameters' runtime values, beside the constant
# ceiling. `_own` names the guarded function's own parameters, read off the
# specialization its `generic` was called through; `_receiver` names its class's,
# read off the instance the method was called on — the one the specialization
# was bound to, or else the first argument. a parameter of an enclosing reified
# function is neither: the lambda closes over it where the guard is evaluated.
# whenever a value cannot be read, the guard tests the ceiling
def _by_raises(_allowed, _name, _resolve=None, _own=(), _receiver=()):
    import functools
    import inspect

    def _target(_generic, _args):
        if _resolve is None:
            return _allowed
        try:
            _values = []
            if _own:
                _bound = _bind_type_params(
                    _generic.fn.__type_params__,
                    _generic.args or (),
                    _generic.fields or {},
                    _generic.fn.__name__,
                )
                _values.extend(_bound[_param] for _param in _own)
            if _receiver:
                _instance = None if _generic is None else _generic.instance
                _owner = _args[0] if _instance is None else _instance
                _values.extend(_type_argument(_owner, _param) for _param in _receiver)
        except (TypeError, KeyError, IndexError):
            # a parameter nothing answered for: what the declaration says without
            # it is the ceiling. binding the type arguments and reading one off a
            # receiver raise `TypeError`, a parameter the specialization left
            # unbound `KeyError`, and a method called with no receiver `IndexError`
            return _allowed
        # the lambda only reads the values and the names the module binds, so
        # anything it raises is a lowering that names something wrong, and is not
        # mistaken for a value nothing answered for
        return _resolve(*_values)

    def _check(_exc, _generic, _args):
        if not _by_isinstance(_exc, _target(_generic, _args), _allowed):
            raise AssertionError(
                f"{_name} raised {type(_exc).__name__}, which its `raises` clause does not include"
            ) from _exc

    def _wrap(_shape, _call, _generic):
        if inspect.isasyncgenfunction(_shape):
            @functools.wraps(_shape)
            async def _wrapper(*_args, **_kwargs):
                try:
                    async for _item in _call(*_args, **_kwargs):
                        yield _item
                except BaseException as _exc:
                    _check(_exc, _generic, _args)
                    raise
        elif inspect.iscoroutinefunction(_shape):
            @functools.wraps(_shape)
            async def _wrapper(*_args, **_kwargs):
                try:
                    return await _call(*_args, **_kwargs)
                except BaseException as _exc:
                    _check(_exc, _generic, _args)
                    raise
        elif inspect.isgeneratorfunction(_shape):
            @functools.wraps(_shape)
            def _wrapper(*_args, **_kwargs):
                try:
                    yield from _call(*_args, **_kwargs)
                except BaseException as _exc:
                    _check(_exc, _generic, _args)
                    raise
        else:
            @functools.wraps(_shape)
            def _wrapper(*_args, **_kwargs):
                try:
                    return _call(*_args, **_kwargs)
                except BaseException as _exc:
                    _check(_exc, _generic, _args)
                    raise
        return _wrapper

    def _decorate(_fn):
        # a reified generic is specialized after it is decorated (`f[int](…)`),
        # so the guard has to keep answering the subscript, and it reads the
        # type arguments off the specialization that subscript produces
        if getattr(_fn, "__by_generic__", False):
            return _by_guarded_generic(_fn, _wrap)
        return _wrap(_fn, _fn, None)

    return _decorate


def _by_isinstance(_value, _target, _fallback):
    # a type argument is whatever the caller wrote, and `isinstance` refuses a
    # subscripted generic. the shallow test is its origin, the way `list[str]`
    # is tested as `list`, and past that the ceiling
    try:
        return isinstance(_value, _target)
    except TypeError:
        pass
    try:
        return isinstance(_value, _by_runtime_classes(_target))
    except TypeError:
        return isinstance(_value, _fallback)


def _by_runtime_classes(_target):
    import typing

    if isinstance(_target, tuple):
        return tuple(_by_runtime_classes(_member) for _member in _target)
    return typing.get_origin(_target) or _target


class _by_guarded_generic:
    # a guarded reified generic: still subscriptable, still a descriptor, and
    # documented by the function it wraps
    __doc__ = property(lambda self: self._by_inner.__doc__)

    def __reduce_ex__(self, protocol):
        return self._by_inner.__reduce_ex__(protocol)

    def __init__(self, _inner, _wrap):
        self._by_inner = _inner
        self._by_wrap = _wrap
        self._by_call = None

    def __repr__(self):
        return repr(self._by_inner)

    def __getattr__(self, _name):
        if _name in ("_by_inner", "_by_wrap", "_by_call"):
            raise AttributeError(_name)
        return getattr(self._by_inner, _name)

    def __get__(self, _obj, _objtype=None):
        return _by_guarded_generic(self._by_inner.__get__(_obj, _objtype), self._by_wrap)

    def __getitem__(self, *_items, **_fields):
        return _by_guarded_generic(
            self._by_inner.__getitem__(*_items, **_fields), self._by_wrap
        )

    def __call__(self, *_args, **_kwargs):
        # the wrapper's shape comes from the function this specialization holds,
        # so it is built once for it rather than once per call
        if self._by_call is None:
            _inner = self._by_inner
            self._by_call = self._by_wrap(_inner.fn, _inner, _inner)
        return self._by_call(*_args, **_kwargs)


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


# a t-string field's source text, as the interpreter running it reports one
#
# a lowered field's text keeps the whitespace before the `=`, conversion, format spec
# or `}` that ends the field, which is what python reports from 3.14.8 on. 3.14.0 to
# 3.14.7 dropped it (python/cpython#154719), so there it is dropped here too, or a
# lowered field would report different text from an untouched field beside it. an
# interpreter older than 3.14 has no t-strings and so no form of its own, and keeps
# the newer one
def _by_field_text(_text):
    from sys import version_info

    if (3, 14) <= version_info < (3, 14, 8):
        return _text.rstrip()
    return _text


# PEP 750 `Template` / `Interpolation` polyfill for runtimes before 3.14
#
# matches the `string.templatelib` shape a tag relies on: `Template.strings`
# is the literal segments (always one more than the interpolations),
# `Template.interpolations` is the replacement fields, and `Template.values`
# is their evaluated values. iterating a `Template` yields the segments and
# interpolations interleaved in source order, the same as the stdlib type
#
# the lowering builds every `_Interpolation` with its field's source text in the form
# python 3.14.8 reports, which `_by_field_text` matches to the interpreter
class _Interpolation:
    def __init__(self, value, expression, conversion=None, format_spec=""):
        self.value = value
        self.expression = _by_field_text(expression)
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


# a `t"..."` the author wrote, carrying the text the author wrote for each of its
# fields rather than the text this file was lowered to
#
# `Interpolation.expression` is the source of the field, which python reads off the
# compiled file — `t"{a!}"` would hand a reader `_force_unwrap(a)`. `_exprs` runs
# parallel to the fields: a string is the author's own text for that field, and
# `None` a field nothing lowered, which keeps the interpolation python built and so
# stays byte-identical to what python would have reported
#
# the author's text is written as python 3.14.8 reports it, which `_by_field_text`
# matches to the interpreter running it
#
# the import is made here rather than at the top of the file because this module is
# shared with runtimes that have no `string.templatelib`. only a t-string can reach
# this helper, and a t-string needs 3.14
def _by_template(_t, _exprs):
    from string.templatelib import Interpolation, Template

    _strings = _t.strings
    _parts = []
    for _index, _field in enumerate(_t.interpolations):
        _parts.append(_strings[_index])
        _expr = _exprs[_index]
        _parts.append(
            _field
            if _expr is None
            else Interpolation(
                _field.value,
                _by_field_text(_expr),
                _field.conversion,
                _field.format_spec,
            )
        )
    _parts.append(_strings[-1])
    return Template(*_parts)


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
#
# `forwarding` is for a parameter the body only writes into a specialization it
# records (`A[T](t)`). a call the transpiler has no spelling for is still legal
# there, so the slot is left alone and the cell keeps the `TypeVar` itself
class generic:
    # what tells another lowering's wrapper that this is a reified generic and
    # not the plain function it forwards to — the `raises` guard has to keep the
    # specialization subscript working
    __by_generic__ = True
    __doc__ = property(lambda self: self.fn.__doc__)

    def __init__(self, fn, args=None, instance=None, fields=None, optional=()):
        self.fn = fn
        self.args = args
        self.instance = instance
        self.fields = fields
        self.optional = optional

    @classmethod
    def forwarding(cls, *optional):
        return lambda fn: cls(fn, optional=optional)

    def __repr__(self):
        return f"<generic {self.fn!r}>"

    # the function can't be pickled, since its name holds this rather than it. this can, by that name
    def __reduce_ex__(self, protocol):
        fn = self.fn
        place = (fn.__module__, fn.__qualname__)
        try:
            _by_generic_named(*place)
        except (AttributeError, ImportError) as error:
            import pickle
            raise pickle.PicklingError(f"Can't pickle {self!r}: {error}") from error
        items = self.args
        return (
            _by_generic_named,
            (*place, self.instance, None if items is None else tuple(map(_by_pickled, items)), self.fields),
        )

    def __getattr__(self, name):
        if name == "fn":
            raise AttributeError(name)
        return getattr(self.fn, name)

    def __get__(self, obj, objtype=None):
        if obj is None:
            return self
        return generic(self.fn, self.args, obj, self.fields, self.optional)

    def __getitem__(self, *items, **fields):
        if self.args is not None or self.fields is not None:
            raise TypeError("type arguments already specified")
        if len(items) == 1 and isinstance(items[0], tuple):
            items = items[0]
        # reject a bad arity here, not at the call
        _bind_type_params(self.fn.__type_params__, items, fields, self.fn.__name__)
        return generic(self.fn, items, self.instance, fields, self.optional)

    def __call__(self, *args, **kwargs):
        from types import CellType, FunctionType

        fn = self.fn
        code = fn.__code__
        values = _bind_type_params(
            fn.__type_params__, self.args or (), self.fields or {}, fn.__name__
        )
        for param in fn.__type_params__:
            name = param.__name__
            if name not in values and name in code.co_freevars and name not in self.optional:
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
            CellType(values[name]) if name in values
            else CellType(_ByNotRecorded) if name in self.optional
            else cell
            for name, cell in zip(code.co_freevars, fn.__closure__ or ())
        )
        temp_fn = FunctionType(code, fn.__globals__, fn.__name__, fn.__defaults__, closure)
        temp_fn.__kwdefaults__ = fn.__kwdefaults__
        temp_fn.__qualname__ = fn.__qualname__
        if self.instance is not None:
            return temp_fn(self.instance, *args, **kwargs)
        return temp_fn(*args, **kwargs)


# what a pickled reified function is read back from
def _by_generic_named(module, qualified, instance=None, items=None, fields=None):
    import importlib
    found = importlib.import_module(module)
    for part in qualified.split("."):
        found = getattr(found, part)
    if instance is not None:
        found = found.__get__(instance, type(instance))
    if items is not None or fields is not None:
        found = found.__getitem__(items or (), **(fields or {}))
    return found


# what the cell of a forwarded type parameter holds when the call supplied nothing for it. a
# value built with it, `Box[_ByNotRecorded](t)`, records nothing, and being a module-level class
# it can still be pickled, which the `TypeVar` left in the cell could not be
class _ByNotRecorded:
    pass


# `origin[args]` for a class the checker knows as generic but whose runtime may
# have no subscript: a c type only has one if cpython wrote it by hand. a
# `types.GenericAlias` is what the subscript would have been
def _by_subscript(origin, *args):
    args = args[0] if len(args) == 1 else args
    if hasattr(origin, "__class_getitem__") or hasattr(type(origin), "__getitem__"):
        return origin[args]
    from types import GenericAlias
    return GenericAlias(origin, args)


# `origin[args]` read as a type, for a class that answers a subscript for itself: whatever its
# metaclass `__getitem__` or `__class_getitem__` returns, `U[int]` in `x is U[int]` is the type
def _by_specialization(origin, *args):
    from types import GenericAlias
    return GenericAlias(origin, args[0] if len(args) == 1 else args)


# a bare call of a method that only forwards its type parameters. an override may
# forward none, which makes it a plain method with no specialization to take
def _by_specialized(callee, *items, **fields):
    if getattr(callee, "__by_generic__", False):
        return callee.__getitem__(*items, **fields)
    return callee


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
    if isinstance(annotation, _ByProjection):
        return _by_project(annotation.__by_variance__, _by_subst(annotation.__by_projected__, mapping))
    # what a literal lists are its values, and a value is no type to replace
    if _by_literal_args(annotation) is not None:
        return annotation
    args = getattr(annotation, "__args__", ())
    if not args:
        return annotation
    replaced = []
    for arg in args:
        # a `*Ts` becomes the run of arguments it stands for
        pack = _by_unpacked(arg)
        try:
            run = mapping.get(pack)
        except TypeError:
            run = None
        if isinstance(run, tuple):
            replaced.extend(run)
        else:
            replaced.append(_by_subst(arg, mapping))
    replaced = tuple(replaced)
    return annotation if replaced == args else _by_rebuilt(annotation, replaced)

def _by_rebuilt(alias, args):
    # `alias` with `args` for its arguments, which each kind of alias takes in a shape of its own
    import typing
    from collections.abc import Callable
    from types import GenericAlias
    origin = getattr(alias, "__origin__", None)
    if _by_union_args(alias) is not None:
        return typing.Union[args]
    if hasattr(alias, "copy_with"):
        return alias.copy_with(args)
    if origin is Callable:
        params = ... if args[:-1] == (Ellipsis,) else list(args[:-1])
        return Callable[params, args[-1]]
    # a reified class's specialization is a class. anything else that subscripts is asked nothing:
    # a class may answer a subscript for itself with whatever it likes
    if getattr(origin, "__by_reified_class__", False):
        return origin[args]
    return GenericAlias(origin, args)

def _by_unpacked(element):
    # what a `*` in a list of type arguments unpacks, `*Ts` or `*tuple[int, ...]`, and `None` for
    # an argument that isn't one
    import typing
    if getattr(element, "__unpacked__", False):
        return element
    if getattr(element, "__origin__", None) is getattr(typing, "Unpack", _by_absent):
        return element.__args__[0]
    return None

def _by_type_params(klass):
    # a class the pep 695 polyfill wrote declares its parameters through
    # `Generic[...]`, which records them in `__parameters__` and leaves
    # `__type_params__` empty, or absent before 3.12. the class of a function has the
    # attribute too, as the descriptor each function reads its own through
    params = getattr(klass, "__type_params__", ()) or getattr(klass, "__parameters__", ())
    return params if isinstance(params, tuple) else ()

def _by_param_mapping(params, args):
    # each of `params` with what `args` binds it to, a `*Ts` with the run it takes
    import typing
    variadic = getattr(typing, "TypeVarTuple", ())
    mapping = {}
    for index, param in enumerate(params):
        if isinstance(param, variadic):
            end = len(args) - (len(params) - index - 1)
            mapping[param] = tuple(args[index:end])
            for later, arg in zip(params[index + 1:], args[end:]):
                mapping[later] = arg
            return mapping
        if index < len(args):
            mapping[param] = args[index]
    return mapping

def _by_tuple_element(args):
    # what a tuple listing `args` specializes its base with: any of its elements
    import typing
    head, run, tail = _by_tuple_parts(args)
    elements = (*head, *(() if run is None else (run,)), *tail)
    return typing.Union[elements] if elements else typing.NoReturn

def _by_bound_forward(annotation, module):
    # `annotation` with each forward reference in it saying that its name is one of `module`'s.
    # a base is written with one saying nothing, and is said for by the class it is a base of
    import typing
    if isinstance(annotation, str):
        return annotation if isinstance(annotation, _ByForwardName) else _ByForwardName(annotation, module)
    if isinstance(annotation, typing.ForwardRef):
        name = annotation.__forward_arg__
        if isinstance(name, _ByForwardName) or getattr(annotation, "__forward_module__", None) is not None:
            return annotation
        return typing.ForwardRef(_ByForwardName(name, module))
    if _by_literal_args(annotation) is not None:
        return annotation
    args = getattr(annotation, "__args__", ())
    if not args:
        return annotation
    replaced = tuple(_by_bound_forward(arg, module) for arg in args)
    # what was bound is equal to what wasn't, by design
    if all(new is old for new, old in zip(replaced, args)):
        return annotation
    return _by_rebuilt(annotation, replaced)

def _by_specialize(alias, origin, depth=0):
    # the arguments with which `alias` satisfies `origin`, resolved *down the
    # declared base chain* rather than assumed to line up positionally. a base
    # that fixes or reorders its arguments is then followed faithfully:
    # `class Odd[T](list[int])` is a `list[int]` whatever `T` is, and
    # `class Swap[A, B](dict[B, A])` specializes `dict` in the other order. `None` when it
    # doesn't reach `origin`, or reaches it with arguments nothing says
    if depth > 16:
        return None
    alias = _by_type_normal(alias)
    klass = getattr(alias, "__origin__", alias)
    if not isinstance(klass, type):
        return None
    import typing
    args = getattr(alias, "__args__", None)
    row = _by_told_row(klass)
    params = _by_type_params(klass)
    if args is None:
        if row is not None:
            args = _by_told_defaults(row)
        else:
            defaulted = _by_type_param_defaults(params)
            if defaulted is not params:
                args = defaulted
    if klass is origin:
        return args
    if issubclass(klass, tuple):
        if klass is not tuple:
            elements, known = _by_tuple_args(alias)
            if known and elements is not None and hasattr(klass, "_fields"):
                return _by_specialize(tuple[elements], origin, depth + 1)
        elif args is not None:
            # the arguments of a tuple are its elements. its base has one
            args = (_by_tuple_element(args),)
    if row is not None:
        for base in row[3]:
            try:
                told = _by_told_type(base, () if args is None else args)
            except IndexError:
                # a parameter of a class written with no arguments
                continue
            found = _by_specialize(told, origin, depth + 1)
            if found is not None:
                return found
        return None
    mapping = _by_param_mapping(params, args or ())
    bases = klass.__dict__.get("__orig_bases__")
    if bases is None:
        # a class inheriting only plain classes records no `__orig_bases__`
        bases = getattr(klass, "__bases__", ())
    for base in bases:
        base = _by_bound_forward(base, klass.__module__)
        found = _by_specialize(_by_subst(base, mapping) if mapping else base, origin, depth + 1)
        if found is not None:
            return found
    if _by_is_typed_dict(klass):
        from collections.abc import Mapping
        return _by_specialize(Mapping[str, object], origin, depth + 1)
    # the declared bases don't reach `origin`: a builtin registered as a *virtual*
    # subclass of an abc (`list` for `Sequence`) has no base to walk. its
    # arguments do line up positionally once membership is established. this runs
    # only after resolution has failed, so it applies to the already-resolved base
    # (`list[int]`), never to a subclass that fixes or reorders arguments
    if args and isinstance(origin, type) and _by_declared_as(klass, origin):
        return args
    return None

def _by_declared_as(klass, origin):
    # whether `klass` was declared to be an `origin`, by a base or by registering with a
    # class that is one. an abc's `__subclasshook__` admits whatever has the methods, which
    # says nothing about the arguments: `enumerate[int]` has `__next__`, and is an
    # `Iterator[tuple[int, int]]`
    if origin in klass.__mro__:
        return True
    pending = [origin]
    while pending:
        abc = pending.pop()
        try:
            if "__subclasshook__" not in abc.__dict__ and issubclass(klass, abc):
                return True
            pending.extend(type.__subclasses__(abc))
        except TypeError:
            # a protocol `issubclass` refuses shows nothing
            pass
    return False

def _by_recorded_alias(value):
    # the specialization `value` records, if it records one. a function that
    # forwards a type parameter and was called with no type argument for it still
    # builds `A[T](…)`, and an alias with a parameter left in it says nothing
    reified = getattr(value, "__orig_class__", None)
    if getattr(reified, "__parameters__", ()) or _by_mentions_not_recorded(reified):
        return None
    return reified

def _by_mentions_not_recorded(alias):
    return alias is _ByNotRecorded or any(
        _by_mentions_not_recorded(arg) for arg in getattr(alias, "__args__", ())
    )

def _by_generic_args(value, origin):
    # an explicit `A[int]()` records its specialization on the instance;
    # otherwise the class itself is the starting point and any pep 696 defaults
    # stand in for the arguments it was constructed with
    reified = _by_recorded_alias(value)
    found = _by_specialize(reified if reified is not None else type(value), origin)
    return [found] if found is not None else []

def _parametric_is(value, alias, variances):
    alias = _by_type_normal(alias)
    origin = getattr(alias, "__origin__", alias)
    target_args = getattr(alias, "__args__", ())
    # what a class is an instance of says nothing of `type`'s argument: the class itself does
    if origin is type and target_args:
        return isinstance(value, type) and _by_is_sub(value, target_args[0])
    if not isinstance(value, origin):
        return False
    # a tuple's arguments are its elements, however many of them the target lists
    if origin is tuple and hasattr(alias, "__args__"):
        recorded = _by_recorded_alias(value)
        if recorded is None and _by_tuple_args(type(value)) == (None, True):
            return _by_tuple_value_is(value, alias)
        return _by_tuple_is_sub(recorded if recorded is not None else type(value), alias, (), False)
    if len(target_args) != len(variances):
        return False
    # the base of a tuple is specialized with what its elements are, as a tuple's own target is
    if type(value) is tuple and variances == (1,):
        mapped = _by_specialize(tuple[_ByUnseen], origin)
        if mapped == (_ByUnseen,):
            return all(_by_cell_is(element, target_args[0]) for element in value)
    return any(
        _by_arguments_ok(origin, reified_args, target_args, variances)
        for reified_args in _by_generic_args(value, origin)
    )

def _by_tuple_value_is(value, alias):
    # a tuple records no arguments, but its elements are there to ask
    head, run, tail = _by_tuple_parts(alias.__args__)
    if len(value) < len(head) + len(tail) or (run is None and len(value) != len(head)):
        return False
    end = len(value) - len(tail)
    return (
        all(map(_by_cell_is, value[:len(head)], head))
        and all(_by_cell_is(element, run) for element in value[len(head):end])
        and all(map(_by_cell_is, value[end:], tail))
    )

def _by_cell_is(value, cell):
    # `value is T`, for a reified `T`. the cell holds whatever the call supplied, and
    # `isinstance` refuses `None` and a subscripted class outright
    import typing
    cell = _by_type_normal(cell)
    if cell is type(None):
        return value is None
    if cell is typing.Any:
        return True
    if _by_is_never(cell):
        return False
    if isinstance(cell, _ByProjection):
        return _by_cell_is(value, cell.__by_projected__)
    if _by_union_args(cell) is not None:
        return any(_by_cell_is(value, arm) for arm in _by_union_args(cell))
    values = _by_literal_args(cell)
    if values is not None:
        return any(type(value) is type(member) and value == member for member in values)
    conjuncts = _by_intersection_args(cell)
    if conjuncts is not None:
        return all(_by_cell_is(value, conjunct) for conjunct in conjuncts)
    # a protocol `isinstance` can't be asked about is asked as the target in place is, of the
    # annotations of the value's class
    origin = getattr(cell, "__origin__", cell)
    if getattr(origin, "_is_protocol", False) and (
        hasattr(cell, "__args__") or not getattr(origin, "_is_runtime_protocol", False)
    ):
        attrs = _by_protocol_attrs(origin)
        if not all(hasattr(value, name) for name in attrs):
            return False
        try:
            return _by_protocol_is(value, _by_protocol_members(cell, attrs))
        except TypeError:
            # what no annotation says is left to the members being there
            return True
    if not hasattr(cell, "__args__"):
        try:
            return isinstance(value, cell)
        except TypeError:
            # a `NewType`, a `TypedDict` or a `LiteralString`: nothing about a value says it is one
            return False
    args = cell.__args__
    return _parametric_is(value, cell, _by_told_variance(cell.__origin__, len(args)))

def _parametric_is_lenient(value, alias, variances):
    # the checked-cast form: a value that records no reification has no
    # arguments to check, so the base class test is the whole guarantee. this is
    # what keeps `[1, 2] cast list[int]` legal while still rejecting a value
    # whose recorded arguments contradict the target
    alias = _by_type_normal(alias)
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

def _by_literal_members(values):
    return {(type(value), value) for value in values}

# a literal type argument (`A[True]` specializes `T` to `Literal[True]`), spelled as a call so that
# it needs no import of its own. a string written below python 3.12 comes as its code points: it
# may be going into an f-string's replacement field, which can hold neither a backslash nor the
# f-string's own quote. the same literal is the same object, which `typing`'s own bounded cache
# doesn't promise
_by_literals = {}

def _by_lit(*values, text=None, data=None):
    if text is not None:
        values += ("".join(map(chr, text)),)
    if data is not None:
        values += (bytes(data),)
    key = tuple((type(value), value) for value in values)
    literal = _by_literals.get(key)
    if literal is None:
        import typing
        literal = _by_literals[key] = typing.Literal[values]
    return literal

def _by_literal_args(t):
    import typing
    return typing.get_args(t) if typing.get_origin(t) is typing.Literal else None

# --- subtyping between runtime types --------------------------------------
# what `_parametric_is` asks of each type argument: the checker's relation, over the objects
# the runtime holds. the variance of a class is the checker's as well, and the runtime learns it
# from `_by_variance`, which a transpiled class wears as a decorator. a class no transpiled
# module defines is told where one subscripts it, since nothing else names it
#
# a use-site projection is a fact about one type argument rather than about the class, so it is
# in the argument: `Box[out object]` is `Box[_by_out(object)]`, which is its own alias, the one
# `typing` caches, and no `Box[object]` can be mistaken for it. everything else the relation reads
# is already in the alias, a forwarded cell included: `Out[T]` evaluates to what `T` holds


def _by_variance_registry():
    # one per process, as the conformance registry is and for the same reason. weak, since a
    # class written in a function is a new class on each call
    import sys
    import types
    import weakref
    module = sys.modules.get("_by_variance_registry")
    if module is None:
        module = types.ModuleType("_by_variance_registry")
        module.table = weakref.WeakKeyDictionary()
        sys.modules["_by_variance_registry"] = module
    return module.table

_by_variances = _by_variance_registry()

def _by_variance(*codes):
    # one code per type parameter: 0 invariant, 1 covariant, 2 contravariant, 3 bivariant
    def told(cls):
        try:
            _by_variances[cls] = codes
        except TypeError:
            # a decorator beneath this one may have replaced the class with what no weak
            # reference can hold, and that is no class to read a type argument of
            pass
        return cls
    return told

# the bases a stub gives a class are in no object the runtime holds: `class str(Sequence[str])` is
# a `str` whose `__bases__` is `(object,)`. a module tells the runtime about each class of a stub it
# reads as a type, and each class those derive from, as one row each: the class, a default or `...`
# for each of its type parameters (`None` if it has none), the members it declares if the checker
# reads it as a protocol (`None` if not), its bases, and what calling it is
#
# a row is data, so that it can be written ahead of anything the module binds, and read by what
# each name means in whichever module it was read from. a class is `"module:qualified.name"`, a
# leading `:` being the module doing the telling. a type is a class, an `int` for a type parameter
# of the row's class, `None`, a tuple of a class and its arguments, a tuple `("|", ...)` of a union's
# arms or `("=", ...)` of a literal's values. what can't be spelled is `"?"`, which names nothing
#
# each way of calling it is `(required, params, rest, returned)`: how many of `params` must be
# given, the type of each positional parameter, that of a `*args` or `None`, and the type returned
def _by_told_registry():
    # one per process, as the conformance registry is and for the same reason. a row is pending
    # until the class it describes can be looked up: a module imported lazily has no class yet
    import sys
    import types
    module = sys.modules.get("_by_told_registry")
    if module is None:
        module = types.ModuleType("_by_told_registry")
        module.pending = {}
        module.rows = {}
        sys.modules["_by_told_registry"] = module
    return module

_by_told_rows = _by_told_registry()

# a class a stub has that nothing at runtime is: it is a subtype of nothing else
class _ByUnseen:
    pass

def _by_told(teller, *rows):
    def absolute(part):
        if isinstance(part, str) and part.startswith(":"):
            return teller + part
        if isinstance(part, tuple):
            return tuple(map(absolute, part))
        return part
    for row in rows:
        row = absolute(row)
        _by_told_rows.pending[row[0]] = row

def _by_told_class(path):
    # the class `path` names, or `None` if nothing at runtime is, or isn't yet. `typing` names the
    # classes of `collections.abc` as aliases of them
    import sys
    module, _, qualified = path.partition(":")
    found = sys.modules.get(module)
    if found is None:
        return None
    try:
        for part in qualified.split("."):
            found = getattr(found, part)
    except AttributeError:
        return None
    return getattr(found, "__origin__", found)

def _by_told_row(klass):
    pending = _by_told_rows.pending
    if pending:
        for path in list(pending):
            found = _by_told_class(path)
            if found is not None:
                _by_told_rows.rows[found] = pending.pop(path)
    try:
        return _by_told_rows.rows.get(klass)
    except TypeError:
        return None

def _by_told_defaults(row):
    # the arguments a class told about has when it is written with none, if they all have one
    params = row[1]
    if not params or any(param is Ellipsis for param in params):
        return None
    return tuple(_by_told_type(param, ()) for param in params)

def _by_told_type(part, args):
    # the type `part` stands for, with `args` for the parameters of the class it is about
    import typing
    from types import GenericAlias
    if part is None:
        return type(None)
    if part is Ellipsis:
        return Ellipsis
    if isinstance(part, int):
        return args[part]
    if isinstance(part, str):
        found = _by_told_class(part)
        return _ByUnseen if found is None else found
    head = part[0]
    if head == "=":
        return typing.Literal[part[1:]]
    arguments = tuple(_by_told_type(element, args) for element in part[1:])
    if head == "|":
        return typing.Union[arguments]
    found = _by_told_class(head)
    return _ByUnseen if found is None else GenericAlias(found, arguments)

def _by_told_variance(origin, count):
    # the code of each of `count` type arguments of `origin`. the checker told the runtime or
    # it did not: what a class that was never told answers is what its parameters declare,
    # which for a class no transpiled module wrote is `TypeVar(covariant=True)` read back, and
    # for a pep 695 parameter of one, whose variance only a checker can infer, is invariant
    try:
        codes = _by_variances.get(origin)
    except TypeError:
        codes = None
    params = _by_type_params(origin)
    if codes is None:
        codes = tuple(
            1 if getattr(param, "__covariant__", False)
            else 2 if getattr(param, "__contravariant__", False)
            else 0
            for param in params
        )
    if len(codes) == count:
        return codes
    # a `*Ts` stands for the run of arguments it absorbs, each varying as it does
    import typing
    variadic = getattr(typing, "TypeVarTuple", ())
    for index, param in enumerate(params):
        if isinstance(param, variadic) and len(codes) == len(params) and len(params) <= count + 1:
            return codes[:index] + codes[index:index + 1] * (count - len(params) + 1) + codes[index + 1:]
    return (0,) * count

# what `out T` and `in T` are at runtime, as a type argument
class _ByProjection(type):
    types = {}

    def __call__(cls, *args, **kwargs):
        return cls.__by_projected__(*args, **kwargs)


__import__("copyreg").pickle(
    _ByProjection, lambda projection: (_by_project, (projection.__by_variance__, projection.__by_projected__))
)


def _by_project(variance, inner):
    # memoized as `_by_lit` is, so the same projection is the same type argument and `typing`
    # hands back the same alias for it
    key = (variance, inner)
    projection = _ByProjection.types.get(key)
    if projection is None:
        written = inner.__qualname__ if isinstance(inner, type) else repr(inner)
        keyword = {1: "out", 2: "in"}[variance]
        projection = _ByProjection(
            f"{keyword} {written}",
            (),
            {"__by_variance__": variance, "__by_projected__": inner, "__module__": "builtins"},
        )
        _ByProjection.types[key] = projection
    return projection

def _by_out(inner):
    return _by_project(1, inner)

def _by_in(inner):
    return _by_project(2, inner)

# a forward reference is a string, and nothing in `"F"` says whose `F` it is. this one does, and is
# otherwise the same string, in whatever `typing` makes of it, so that what reads the alias it is
# in reads what the same alias holds where it says nothing
class _ByForwardName(str):
    def __new__(cls, name, module=None):
        self = super().__new__(cls, name)
        self.__by_module__ = module
        return self

def _by_forward(specialization, module):
    # a specialization whose type arguments python evaluates before what they name is bound,
    # which a test reads once it is
    return _by_bound_forward(specialization, module)

def _by_forward_value(ref):
    # what a forward reference names now, or `_by_absent` if that can't be read yet, or can't be
    # read at all: a name local to a function has no module to look it up in
    import sys
    import typing
    name = ref.__forward_arg__ if isinstance(ref, typing.ForwardRef) else ref
    module = sys.modules.get(
        getattr(name, "__by_module__", None) or getattr(ref, "__forward_module__", None)
    )
    if module is None:
        return _by_absent
    try:
        return eval(str(name), vars(module))
    except Exception:
        return _by_absent

def _by_type_normal(t):
    # what a type stands for once what only spells it is gone: an alias, a metadata wrapper, the
    # `None` that means `NoneType`, a forward reference, a reified class's specialization. each
    # step strictly opens one, so the bound only stops an alias that is its own value
    import typing
    for _ in range(64):
        if t is None:
            return type(None)
        if isinstance(t, (str, typing.ForwardRef)):
            named = _by_forward_value(t)
            if named is _by_absent:
                return t
            t = named
            continue
        origin = getattr(t, "__origin__", None)
        if hasattr(t, "__metadata__"):
            t = origin
        elif hasattr(origin, "__value__"):
            t = _by_subst(origin.__value__, dict(zip(origin.__type_params__, t.__args__)))
        elif hasattr(t, "__value__"):
            t = t.__value__
        elif _by_alias(t) is not t:
            t = _by_alias(t)
        else:
            return t
    return t

def _by_intersection_args(t):
    # asked of the class rather than by identity: a module with nowhere to import this file from
    # has its own copy of it
    origin = getattr(t, "__origin__", None)
    return t.__args__ if getattr(origin, "__by_intersection__", False) else None

def _by_union_args(t):
    import types
    import typing
    if isinstance(t, getattr(types, "UnionType", ())) or typing.get_origin(t) is typing.Union:
        return t.__args__
    return None

def _by_is_never(t):
    import typing
    return t is typing.NoReturn or t is getattr(typing, "Never", typing.NoReturn)

def _by_finite_members(t):
    # the values a class holds, where it is the same type as the union of their literal types
    import enum
    if t is bool:
        return (True, False)
    if isinstance(t, type) and issubclass(t, enum.Enum):
        return tuple(t)
    return None

def _by_value_is_sub(value, b, seen):
    import typing
    arms = _by_union_args(b)
    if arms is not None:
        return any(_by_value_is_sub(value, _by_type_normal(arm), seen) for arm in arms)
    values = _by_literal_args(b)
    if values is not None:
        return (type(value), value) in _by_literal_members(values)
    if b is getattr(typing, "LiteralString", _by_absent):
        return type(value) is str
    return _by_is_sub(type(value), b, seen)

def _by_is_typed_dict(klass):
    import typing
    return getattr(typing, "is_typeddict", lambda _: False)(klass)

def _by_has_ancestor(klass, target, depth=0):
    # whether `target` is among the classes `klass` derives from, as the checker reads them. a
    # class told about has the bases its stub says. a `TypedDict` is a `Mapping[str, object]`,
    # whatever the runtime's mro says
    if klass is target:
        return True
    if depth > 32:
        return False
    row = _by_told_row(klass)
    if row is not None:
        bases = [_by_told_class(base if isinstance(base, str) else base[0]) for base in row[3]]
    elif _by_is_typed_dict(klass):
        from collections.abc import Mapping
        bases = [
            base
            for base in getattr(klass, "__orig_bases__", ())
            if _by_is_typed_dict(base)
        ] + [Mapping]
    else:
        bases = klass.__bases__
    return any(
        isinstance(base, type) and _by_has_ancestor(base, target, depth + 1)
        for base in bases
    )

def _by_protocol_attrs(klass):
    # the members `klass` declares if the checker reads it as a protocol, which a class of the
    # standard library may be only in its stub, and `None` if it doesn't
    row = _by_told_row(klass)
    if row is not None:
        return None if row[2] is None else frozenset(row[2])
    if not getattr(klass, "_is_protocol", False):
        return None
    attrs = getattr(klass, "__protocol_attrs__", None)
    if attrs is None:
        import typing
        attrs = typing._get_protocol_attrs(klass)
    return frozenset(attrs)

def _by_protocol_members(protocol, attrs):
    # `attrs`, in the form `_by_protocol_is` reads, as far as the annotations of `protocol` say what
    # each is: a parameter is contravariant and a return covariant, and a data member is invariant
    # unless it is read-only
    import inspect, typing
    klass = getattr(protocol, "__origin__", protocol)
    mapping = _by_param_mapping(_by_type_params(klass), getattr(protocol, "__args__", ()))
    expected = lambda hint: _by_subst(hint, mapping) if mapping else hint
    members = []
    for name in attrs:
        member = getattr(klass, name, None)
        if isinstance(member, property):
            member = member.fget
            try:
                hints = typing.get_type_hints(member)
            except Exception:
                continue
            if "return" in hints:
                members.append(("attr", name, expected(hints["return"]), 1))
        elif callable(member):
            try:
                hints = typing.get_type_hints(member)
                parameters = list(inspect.signature(member).parameters.values())[1:]
            except Exception:
                continue
            if all(p.name in hints for p in parameters) and "return" in hints:
                members.append((
                    "method",
                    name,
                    [(expected(hints[p.name]), 2) for p in parameters],
                    (expected(hints["return"]), 1),
                ))
        else:
            annotation = _by_member_annotation(klass, name)
            if annotation is not _by_proto_missing:
                members.append(("attr", name, expected(annotation), 0))
    return members

def _by_class_has_members(a, b, attrs):
    # whether the class `a` has the members `attrs` of the protocol `b`, of the types they are
    # where each says what that is
    if not all(
        getattr(a, name, None) is not None
        or _by_member_annotation(a, name) is not _by_proto_missing
        for name in attrs
    ):
        return False
    # a class of a stub has no annotations to compare, and what none says is left to the member
    # being there
    import typing
    def annotated(member):
        found = getattr(a, member[1], None)
        try:
            return callable(found) and bool(typing.get_type_hints(found)) if member[0] == "method" \
                else _by_member_annotation(a, member[1]) is not _by_proto_missing
        except Exception:
            return False
    members = [member for member in _by_protocol_members(b, attrs) if annotated(member)]
    try:
        return _by_protocol_class_is(a, members, hasattr)
    except TypeError:
        return True

def _by_class_is_sub(a, b):
    # a class relates to another through the bases the checker reads, and to a protocol through
    # its members. being registered with an abc is something the runtime says and the checker
    # doesn't, so a class with no `__subclasshook__` of its own only has what derives from it
    if _by_has_ancestor(a, b):
        return True
    attrs = _by_protocol_attrs(b)
    if attrs is None and (_by_told_row(b) is not None or "__subclasshook__" not in b.__dict__):
        return False
    # a protocol has the members it declares, and not whatever every object has
    declared = _by_protocol_attrs(a)
    if declared is not None:
        return attrs is not None and attrs <= declared
    if attrs is None:
        try:
            return issubclass(a, b)
        except TypeError:
            return False
    try:
        if not issubclass(a, b):
            return False
    except TypeError:
        # a protocol that isn't runtime checkable, or one with data members
        pass
    return _by_class_has_members(a, b, attrs)

def _by_tuple_parts(args):
    # a tuple's elements as what comes before its unbounded run, the run, and what comes after
    if args == ((),):
        args = ()
    if len(args) == 2 and args[1] is Ellipsis:
        return (), args[0], ()
    before, run, after = [], None, []
    for element in args:
        unpacked = _by_unpacked(element)
        if getattr(unpacked, "__origin__", None) is tuple:
            head, middle, tail = _by_tuple_parts(unpacked.__args__)
        else:
            head, middle, tail = (element,), None, ()
        if run is None:
            before.extend(head)
            run = middle
            if run is not None:
                after.extend(tail)
        else:
            after.extend(head)
    return tuple(before), run, tuple(after)

def _by_tuple_args(a):
    # the arguments a tuple type lists, and whether it lists them at all: only `tuple` itself
    # says nothing, and it is `tuple[Any, ...]`. a class the runtime can't map is `None`
    origin = getattr(a, "__origin__", a)
    if origin is tuple:
        return getattr(a, "__args__", None), True
    fields = getattr(origin, "_fields", None)
    if fields is not None:
        import typing
        try:
            hints = typing.get_type_hints(origin)
            mapping = dict(zip(_by_type_params(origin), getattr(a, "__args__", ())))
            return tuple(_by_subst(hints[field], mapping) for field in fields), True
        except (KeyError, NameError, TypeError):
            return None, False
    args = _by_specialize(a, tuple)
    return args, args is not None

def _by_elements_ok(actual, expected, is_sub):
    # whether a list of elements, either of which may hold an unbounded run, fits another
    head, run, tail = _by_tuple_parts(actual)
    b_head, b_run, b_tail = _by_tuple_parts(expected)
    if b_run is None:
        return (
            run is None
            and len(head) == len(b_head)
            and all(map(is_sub, head, b_head))
        )
    if run is None:
        # a fixed tuple fills the run with what the target's two ends leave of it
        if len(head) < len(b_head) + len(b_tail):
            return False
        split = len(head) - len(b_tail)
        head, tail = head[:split], head[split:]
    if len(head) < len(b_head) or len(tail) < len(b_tail):
        return False
    left, middle = head[:len(b_head)], head[len(b_head):]
    right = tail[len(tail) - len(b_tail):]
    middle += tail[:len(tail) - len(b_tail)]
    if run is not None:
        middle += (run,)
    return (
        all(map(is_sub, left, b_head))
        and all(map(is_sub, right, b_tail))
        and all(is_sub(element, b_run) for element in middle)
    )

def _by_tuple_is_sub(a, b, seen, gradual=True):
    origin = getattr(a, "__origin__", a)
    if not (isinstance(origin, type) and issubclass(origin, tuple)):
        return False
    a_args, known = _by_tuple_args(a)
    if not known:
        return False
    if a_args is None:
        return gradual and origin is tuple
    return _by_elements_ok(a_args, b.__args__, lambda x, y: _by_is_sub(x, y, seen))

def _by_callable_is_sub(a, b, seen):
    from collections.abc import Callable
    if getattr(a, "__origin__", a) is not Callable:
        return False
    a_args = getattr(a, "__args__", None)
    if a_args is None:
        return True
    a_params, a_return = a_args[:-1], a_args[-1]
    b_params, b_return = b.__args__[:-1], b.__args__[-1]
    if not _by_is_sub(a_return, b_return, seen):
        return False
    if a_params == (Ellipsis,) or b_params == (Ellipsis,):
        return True
    return len(a_params) == len(b_params) and all(
        _by_is_sub(y, x, seen) for x, y in zip(a_params, b_params)
    )

def _by_class_call_is_sub(a, b, seen):
    # what calling a class is, is its constructor, which the checker tells the runtime where a
    # `type[C]` is read. `type` and `type[object]` could be any class, and the checker reads
    # calling one as it would calling something it knows nothing of
    args = getattr(a, "__args__", ())
    target = _by_type_normal(args[0]) if args else object
    if target is object:
        return True
    arms = _by_union_args(target)
    if arms is not None:
        return all(_by_class_call_is_sub(type[arm], b, seen) for arm in arms)
    klass = getattr(target, "__origin__", target)
    row = _by_told_row(klass) if isinstance(klass, type) else None
    if row is None:
        return False
    told_args = getattr(target, "__args__", ())
    return any(_by_constructor_is_sub(constructor, told_args, b, seen) for constructor in row[4])

def _by_constructor_is_sub(constructor, args, b, seen):
    required, params, rest, returned = constructor
    b_params, b_return = b.__args__[:-1], b.__args__[-1]
    try:
        if not _by_is_sub(_by_told_type(returned, args), b_return, seen):
            return False
        if b_params == (Ellipsis,):
            return True
        if len(b_params) < required or (rest is None and len(b_params) > len(params)):
            return False
        return all(
            _by_is_sub(
                param,
                _by_told_type(params[index] if index < len(params) else rest, args),
                seen,
            )
            for index, param in enumerate(b_params)
        )
    except IndexError:
        # a parameter of a class written with no arguments
        return False

def _by_is_sub(a, b, seen=()):
    # whether `a` is a subtype of `b`. this has the checker's answer wherever the runtime holds
    # what the checker read, and says no where it doesn't: a test has to earn its `True`
    import typing
    a = _by_type_normal(a)
    b = _by_type_normal(b)
    if a is b or b is object or a is typing.Any or b is typing.Any or _by_is_never(a):
        return True
    if _by_is_never(b):
        return False
    # coming back to a pair still being answered means a recursive alias has been followed
    # all the way round with nothing saying no. the bound is for an alias that grows forever
    pair = (id(a), id(b))
    if pair in seen:
        return True
    if len(seen) > 64:
        return False
    seen += (pair,)
    arms = _by_union_args(a)
    if arms is not None:
        return all(_by_is_sub(arm, b, seen) for arm in arms)
    conjuncts = _by_intersection_args(b)
    if conjuncts is not None:
        return all(_by_is_sub(a, conjunct, seen) for conjunct in conjuncts)
    values = _by_literal_args(a)
    if values is not None:
        return all(_by_value_is_sub(value, b, seen) for value in values)
    conjuncts = _by_intersection_args(a)
    if conjuncts is not None:
        return any(_by_is_sub(conjunct, b, seen) for conjunct in conjuncts)
    if a is getattr(typing, "LiteralString", _by_absent):
        return _by_is_sub(str, b, seen)
    arms = _by_union_args(b)
    if arms is not None and any(_by_is_sub(a, arm, seen) for arm in arms):
        return True
    if arms is not None or _by_literal_args(b) is not None:
        members = _by_finite_members(a)
        return members is not None and all(_by_value_is_sub(value, b, seen) for value in members)
    supertype = getattr(a, "__supertype__", None)
    if supertype is not None:
        return _by_is_sub(supertype, b, seen)
    a_origin = getattr(a, "__origin__", a)
    b_origin = getattr(b, "__origin__", b)
    if not (isinstance(a_origin, type) and isinstance(b_origin, type)):
        return a == b
    if not hasattr(b, "__args__"):
        return _by_class_is_sub(a_origin, b_origin)
    from collections.abc import Callable
    if b_origin is tuple:
        return _by_tuple_is_sub(a, b, seen)
    if b_origin is Callable:
        if a_origin is type:
            return _by_class_call_is_sub(a, b, seen)
        return _by_callable_is_sub(a, b, seen)
    if b_origin is type:
        # `type` is covariant for the language rather than for any parameter it declares, and
        # says nothing of its own when it is written with no argument
        if a_origin is not type:
            return False
        a_class = a.__args__[0] if hasattr(a, "__args__") else object
        return _by_is_sub(a_class, b.__args__[0], seen)
    mapped = _by_specialize(a, b_origin)
    args = b.__args__
    if mapped is None:
        if _by_is_bare_generic(a, a_origin) and _by_class_is_sub(a_origin, b_origin):
            return True
        # a protocol the program wrote says what each of its members is, which one a stub declares
        # doesn't say at runtime
        attrs = None if _by_told_row(b_origin) is not None else _by_protocol_attrs(b_origin)
        return attrs is not None and _by_class_has_members(a_origin, b, attrs)
    return _by_arguments_ok(b_origin, mapped, args, _by_told_variance(b_origin, len(args)), seen)

def _by_arguments_ok(origin, actual, expected, codes, seen=()):
    # whether the type arguments `actual` fit `expected`, as the parameters of `origin` vary, a
    # code each for `expected`. a `*Ts` takes the run the other parameters leave, and either
    # side may leave that open
    params = _by_type_params(origin)
    if not any(_by_unpacked(argument) is not None for argument in (*actual, *expected)):
        return len(actual) == len(expected) == len(codes) and all(
            _by_variance_ok(x, y, code, seen) for x, y, code in zip(actual, expected, codes)
        )
    import typing
    variadic = getattr(typing, "TypeVarTuple", ())
    index = next((i for i, param in enumerate(params) if isinstance(param, variadic)), None)
    if index is None:
        return False
    declared = _by_told_variance(origin, len(params))
    after = len(params) - index - 1
    if min(len(actual), len(expected)) < index + after:
        return False
    ends = (
        all(_by_variance_ok(x, y, code, seen) for x, y, code in zip(actual[:index], expected[:index], declared))
        and all(
            _by_variance_ok(x, y, code, seen)
            for x, y, code in zip(actual[len(actual) - after:], expected[len(expected) - after:], declared[index + 1:])
        )
    )
    return ends and _by_elements_ok(
        actual[index:len(actual) - after],
        expected[index:len(expected) - after],
        lambda x, y: _by_variance_ok(x, y, declared[index], seen),
    )

def _by_is_bare_generic(a, origin):
    # a generic class with no arguments has unknown ones. a class that only inherits from a
    # generic one has whatever its stub fixes them to, which the runtime can't see
    if hasattr(a, "__args__"):
        return False
    own = origin.__dict__
    return "__class_getitem__" in own or bool(own.get("__parameters__") or own.get("__type_params__"))

def _by_projected_variance(declared, source, target):
    # how an argument relates once each side's projection has had its say, `None` for not at
    # all. a declared variance already covers anything a projection could say
    if declared != 0:
        return declared
    if target is None:
        return 0 if source is None else None
    return target if source is None or source == target else None

def _by_variance_ok(actual, expected, variance, seen=()):
    # 0 invariant, 1 covariant (actual <: expected), 2 contravariant (expected <: actual),
    # 3 bivariant (any)
    source = target = None
    if isinstance(actual, _ByProjection):
        source, actual = actual.__by_variance__, actual.__by_projected__
    if isinstance(expected, _ByProjection):
        target, expected = expected.__by_variance__, expected.__by_projected__
    variance = _by_projected_variance(variance, source, target)
    if variance is None:
        return False
    if variance == 3:
        return True
    if variance == 1:
        return _by_is_sub(actual, expected, seen)
    if variance == 2:
        return _by_is_sub(expected, actual, seen)
    return _by_is_sub(actual, expected, seen) and _by_is_sub(expected, actual, seen)

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
    import sys

    # a marker is the object its module binds under the name it carries. no attribute
    # of its own says so — a class of the program's may define any of them — and a
    # value is read without assuming it has any of them
    def marker_source(value):
        try:
            module = sys.modules.get(value.__module__)
            if module is None or getattr(module, value._by_name) is not value:
                return False, None
            return True, value._by_source
        except Exception:
            return False, None

    # a required parameter after a default carries a marker in place of a default
    def has_default(p):
        if p.default is inspect.Parameter.empty:
            return False
        marker, source = marker_source(p.default)
        return not (marker and source is None)

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
        if not has_default(p):
            return False
    # likewise any required keyword-only parameter would break a protocol call
    for p in signature.parameters.values():
        if p.kind == inspect.Parameter.KEYWORD_ONLY and p.default is inspect.Parameter.empty:
            return False
    for (expected, variance), p in zip(params, positional):
        if p.name in hints:
            actual = hints[p.name]
        elif not has_default(p):
            return False
        elif marker_source(p.default)[0]:
            if variance == 3:
                continue
            # the default is evaluated per call, so the signature holds a marker
            # and nothing at runtime records the type the checker read off the
            # default. the marker's own type would answer for it, wrongly
            raise TypeError(
                "cannot check `" + klass.__qualname__ + "." + name + "` against a "
                + "parameterized protocol: parameter `" + p.name + "` has no "
                + "annotation, and its default is evaluated per call, so nothing "
                + "records its type at runtime. annotate it"
            )
        else:
            # a reified default gives the parameter's inferred type at runtime
            actual = type(p.default)
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
    return _by_protocol_class_is(type(value), members, lambda _, name: hasattr(value, name))

def _by_protocol_class_is(klass, members, has):
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
                if has(klass, name):
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
    if _by_recorded_alias(_v) is not None and not _parametric_is(_v, _alias, _variances):
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
# and substitutes through them, so `class B[U](A[list[U]])` specialized as `B[int]`
# answers `T` with `list[int]` and not with `list[U]`. what is bound is keyed by the
# parameter itself, since a subclass's parameter may share the name of the one it
# fills in. `__orig_class__` is carried as a class attribute, which is where the alias
# would have put it, so every reader of a runtime specialization — `_parametric_is`
# included — sees the same thing it saw before
#
# `_type_argument` answers one read. it takes the receiver rather than the
# class so a `classmethod` can pass `cls` and everything else `self`, and the
# class the read is written in, whose parameter it names. it raises rather than
# returning something the parameter would otherwise still name — whether because
# nothing specialized the class or because a base's argument was never filled in
def generic_class(cls):
    cls.__class_getitem__ = classmethod(_specialize)
    cls.__by_reified_class__ = True
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
    named = _bind_type_params(params, args, {}, cls.__name__)
    own = {}
    for param in params:
        if param.__name__ not in named:
            raise TypeError(
                f"too few type arguments for {cls.__name__}: "
                f"no argument for {param.__name__!r}"
            )
        own[param] = named[param.__name__]
    bound = {}
    for base in reversed(cls.__mro__):
        bound.update(base.__dict__.get("__by_type_arguments__") or {})
    bound = {param: _by_bound_through(value, own) for param, value in bound.items()}
    bound.update(own)
    namespace = {
        "__by_type_arguments__": bound,
        "__orig_class__": GenericAlias(cls, args),
        # the specialization declares nothing of its own, so a slotted class
        # stays slotted instead of gaining a `__dict__` here
        "__slots__": (),
        "__reduce_ex__": _by_reduce_specialized,
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


def _by_bound_through(value, own):
    # a pack is a tuple of type arguments rather than a type, and a keyword-variadic one a mapping
    if isinstance(value, tuple):
        return tuple(_by_subst(element, own) for element in value)
    if isinstance(value, dict):
        return {name: _by_subst(element, own) for name, element in value.items()}
    return _by_subst(value, own)


def _by_names_a_parameter(value):
    from typing import TypeVar, TypeVarTuple
    if isinstance(value, (tuple, list)):
        return any(map(_by_names_a_parameter, value))
    # a class holds the parameters it declares rather than any it is left with
    if isinstance(value, type):
        return False
    return isinstance(value, (TypeVar, TypeVarTuple)) or bool(getattr(value, "__parameters__", ()))


def _type_argument(owner, name, declaring=None):
    cls = owner if isinstance(owner, type) else type(owner)
    bound = getattr(cls, "__by_type_arguments__", None) or {}
    value = _by_absent
    # a read from outside a class names the nearest parameter of that name
    for klass in ((declaring,) if declaring is not None else ()) + cls.__mro__:
        param = next(
            (param for param in getattr(klass, "__type_params__", ()) if param.__name__ == name),
            None,
        )
        if param is not None:
            value = bound.get(param, param)
            break
    # a bare class stands for the defaults of the parameters it leaves open
    has_default = getattr(value, "has_default", None)
    if has_default is not None and has_default():
        value = value.__default__
    # a value still naming a type parameter is a base's argument that nothing
    # filled in, which means the instance came from the bare class
    if value is _by_absent or _by_names_a_parameter(value):
        raise TypeError(
            f"{cls.__name__} has no type argument for {name!r}: it was not "
            f"constructed from a specialization"
        )
    return value


# `pickle` looks a class up by name, and no name holds a specialization: `A[int]` calls itself `A`,
# and `A` is another class. so what is pickled has `_ByPickledClass` where it has a specialization,
# which is read back as the subscript it was made by. nothing can say how a class itself pickles
# without a metaclass, and a metaclass of its own would conflict with whatever a subclass of the
# specialization derives from as well
class _ByPickledClass:
    def __init__(self, target):
        self.__by_class__ = target

    # standing where a `__reduce__` puts the class's call, and `copy` calls that without pickling
    def __call__(self, *args, **kwargs):
        return self.__by_class__(*args, **kwargs)

    def __reduce__(self):
        target = self.__by_class__
        if isinstance(target, type):
            alias = target.__dict__["__orig_class__"]
            return (_by_rebuild_pickled, (alias.__origin__, tuple(map(_by_pickled, alias.__args__))))
        arms = _by_union_args(target)
        if arms is not None:
            return (_by_rebuild_pickled, (None, tuple(map(_by_pickled, arms))))
        return (_by_rebuild_pickled, (target.__origin__, tuple(map(_by_pickled, target.__args__))))


def _by_rebuild_pickled(origin, args):
    if origin is None:
        import typing
        return typing.Union[args]
    return origin[args]


def _by_holds_specialization(t):
    if isinstance(t, type):
        return "__by_type_arguments__" in t.__dict__
    if _by_literal_args(t) is not None:
        return False
    return any(map(_by_holds_specialization, getattr(t, "__args__", ())))


def _by_pickled(t):
    return _ByPickledClass(t) if _by_holds_specialization(t) else t


def _by_newobj(cls, *args):
    # `copyreg.__newobj__`, which `pickle` won't take with anything but the class for its first
    # argument. `copy` calls this with what `pickle` would have read back from it
    cls = getattr(cls, "__by_class__", cls)
    return cls.__new__(cls, *args)


def _by_newobj_ex(cls, args, kwargs):
    cls = getattr(cls, "__by_class__", cls)
    return cls.__new__(cls, *args, **kwargs)


def _by_reduce_specialized(self, protocol):
    import copyreg
    reduce = next(
        klass.__dict__["__reduce_ex__"]
        for klass in type(self).__mro__
        if "__reduce_ex__" in klass.__dict__
        and klass.__dict__["__reduce_ex__"] is not _by_reduce_specialized
    )
    reduced = reduce(self, protocol)
    if not isinstance(reduced, tuple):
        return reduced
    call, args = reduced[0], reduced[1]
    if call is copyreg.__newobj__:
        call = _by_newobj
    elif call is copyreg.__newobj_ex__:
        call = _by_newobj_ex
    return (_by_pickled(call), tuple(map(_by_pickled, args)), *reduced[2:])


# --- lazy imports ----------------------------------------------------------
# below python 3.15 there is no `lazy` keyword, so an import is rewritten to a
# call. a module import becomes `_lazy_module`, which binds the real module and
# runs its body on first attribute access
#
# a `from` import binds nothing where it is written. the module holds one
# `_LazyFrom` whose `__dict__` is the module's own namespace, the statement
# registers its names with it (`_lazy_from`), and every read of an imported
# name is written as a read off it. a name already in the namespace is a plain
# dict hit; a missing one is resolved from its module, stored in the namespace
# and returned, so from its first use on the name is the real object, exactly
# as PEP 810 makes it. sharing the namespace is also what keeps patching
# working: `mock.patch("app.svc.fetch")` writes `app.svc`'s namespace, which is
# what every read in `app.svc` looks in first
#
# the module's `__getattr__` and `__dir__` serve the names nothing has read yet
# to other modules: `from app.svc import fetch`, `app.svc.fetch`, `dir(app.svc)`


import builtins as _by_builtins, importlib as _by_il, importlib.util as _by_iu, sys as _by_sys
import _thread as _by_thread
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
        raise ModuleNotFoundError(f"No module named {name!r}", name=name)
    spec.loader = _by_iu.LazyLoader(spec.loader)
    mod = _by_iu.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return _by_sys.modules.setdefault(name, mod)


# the names a module's deferred `from` imports bind. `names` is every one of them,
# so a read of one whose statement has not run yet is what python's global lookup
# gives — the builtin of that name, or a `NameError` — while any other attribute is
# an `AttributeError`, as tools that walk a module's namespace probing what they
# find expect. no deferred name begins with two underscores, so none can be
# mistaken for an attribute of the holder itself
class _LazyFrom:
    __slots__ = ("__dict__", "__by_from__", "__by_resolving__")

    def __init__(self, namespace, names):
        self.__dict__ = namespace
        self.__by_from__ = dict.fromkeys(names)
        self.__by_resolving__ = set()

    def __getattr__(self, name):
        source = self.__by_from__.get(name)
        if source is None:
            if name in self.__by_from__:
                # `if PY2: from itertools import izip as zip` leaves `zip` the
                # builtin where the import did not run. the builtins are the ones
                # the module runs with, a module or its dict
                b = self.__dict__.get("__builtins__", _by_builtins)
                b = b if isinstance(b, dict) else b.__dict__
                if name in b:
                    return b[name]
                error = NameError(f"name {name!r} is not defined")
                # `NameError.name` arrived in 3.10, and this polyfill runs on 3.9
                error.name = name
                raise error
            raise AttributeError(name)
        # while a name resolves, the module's own `__getattr__` does not answer for
        # it: `from . import helper` in a package's `__init__` reads `helper` off the
        # package, which is this module, and has to fall through to importing the
        # submodule rather than resolve itself again
        key = (name, _by_thread.get_ident())
        self.__by_resolving__.add(key)
        try:
            value = _lazy_from_value(*source)
        finally:
            self.__by_resolving__.discard(key)
        self.__dict__[name] = value
        return value

    # the module's `__getattr__`: a deferred name another module reads before
    # this one has
    def __by_getattr__(self, name):
        if (
            self.__by_from__.get(name) is None
            or (name, _by_thread.get_ident()) in self.__by_resolving__
        ):
            raise AttributeError(
                f"module {self.__dict__.get('__name__')!r} has no attribute {name!r}"
            )
        return getattr(self, name)

    # the module's `__dir__`
    def __by_dir__(self):
        names = set(self.__dict__)
        names.update(name for name, source in self.__by_from__.items() if source is not None)
        return sorted(names)


# what `from mod import attr` binds, read as python's import machinery reads it
def _lazy_from_value(mod, attr):
    # the module is needed now, so it is imported as an ordinary import would, and
    # whatever its body raises is the import's error, an `AttributeError` included
    m = _by_sys.modules.get(mod)
    if m is None:
        m = _by_il.import_module(mod)
    else:
        # one another import made lazily runs its body on its first attribute read
        m.__dict__
    try:
        return getattr(m, attr)
    except AttributeError:
        pass
    name = getattr(m, "__name__", mod)
    # a package's submodule nothing has imported yet: `urllib/__init__.py` never
    # imports `parse`, and cpython binds it because `__import__` is handed a
    # fromlist, which imports it. a submodule that does not exist is the name
    # missing; a module its body cannot import is the error that body raised
    if hasattr(m, "__path__"):
        submodule = name + "." + attr
        try:
            _by_il.import_module(submodule)
        except ModuleNotFoundError as error:
            if error.name != submodule or (
                submodule in _by_sys.modules and _by_sys.modules[submodule] is None
            ):
                raise
        try:
            return getattr(m, attr)
        except AttributeError:
            pass
        value = _by_sys.modules.get(submodule)
        if value is not None:
            return value
    # worded as the import machinery words it, down to the module's file: a
    # `from x import y` that fails is something programs catch and report, so the
    # report must not say where the import was written
    path = getattr(m, "__file__", None)
    where = path if isinstance(path, str) else "unknown location"
    if getattr(getattr(m, "__spec__", None), "_initializing", False):
        message = (
            f"cannot import name {attr!r} from partially initialized module {name!r} "
            f"(most likely due to a circular import) ({where})"
        )
    else:
        message = f"cannot import name {attr!r} from {name!r} ({where})"
    error = ImportError(message, name=name, path=path)
    # `name_from` arrived in 3.12, and this polyfill runs on 3.9
    if hasattr(error, "name_from"):
        error.name_from = attr
    raise error


# a `from` import: registers each name it binds, as `{bound: imported}`, with the
# module's `_LazyFrom`. a relative import names its module against the package
# doing the importing, which only that module knows, so it hands `__package__` in
#
# a value the name already holds is from before the statement ran — a reload runs
# the module again over the namespace it had — and the statement replaces it
def _lazy_from(lazy, mod, names, package=None):
    if package is not None:
        mod = _by_iu.resolve_name(mod, package)
    for bound, attr in names.items():
        lazy.__dict__.pop(bound, None)
        lazy.__by_from__[bound] = (mod, attr)


# a type-only marker for `ty_extensions` names, which have no runtime import
# to make. it supports the type-expression operations the language allows on
# them and nothing else


class _TyExtMarker:
    def __class_getitem__(cls, k): return cls


# `A & B` is a type a test can be asked about, so what it is made of is kept
class _ByIntersection:
    __by_intersection__ = True

    def __class_getitem__(cls, k):
        from types import GenericAlias
        return GenericAlias(cls, k)


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


# --- raise expression ------------------------------------------------------
# `f(raise E)` raises partway through evaluating an expression, and python has no
# expression that does that, so the arguments here are the ones `raise` was written
# with. `raise` with none re-raises what is being handled, which a call can still see


def _by_raise(*raised):
    if not raised:
        raise
    if len(raised) == 1:
        raise raised[0]
    raise raised[0] from raised[1]


def _by_no_case_matched():
    # a statement expression's `match` the checker found exhaustive, reached with a value it
    # ruled out. nothing else would say so: the name the value was to be read from is just unbound
    raise TypeError("no case of this `match` matched, though the checker found it exhaustive")


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


# --- slotted dataclasses below python 3.10 -----------------------------------
# a `data class` is a slotted dataclass, and `dataclass(slots=True)` is new in
# 3.10. this is what that option does, to a class `dataclass` has already
# processed: a class cannot take `__slots__` once it exists, so an identical one
# is made with them, its fields' class-level defaults left out (`dataclass` has
# them already) and every zero-argument `super()` pointed at it


def _by_dataclass_getstate(self):
    import dataclasses
    return [getattr(self, f.name) for f in dataclasses.fields(self)]


def _by_dataclass_setstate(self, state):
    import dataclasses
    for f, value in zip(dataclasses.fields(self), state):
        # a frozen dataclass refuses a plain assignment
        object.__setattr__(self, f.name, value)


def _by_dataclass_slots_of(cls):
    slots = cls.__dict__.get("__slots__")
    if slots is None:
        found = []
        if getattr(cls, "__weakrefoffset__", -1) != 0:
            found.append("__weakref__")
        if getattr(cls, "__dictoffset__", -1) != 0:
            found.append("__dict__")
        return found
    if isinstance(slots, str):
        return [slots]
    if hasattr(slots, "__next__"):
        raise TypeError(f"Slots of '{cls.__name__}' cannot be determined")
    return list(slots)


def _by_dataclass_class_cell(f, old, new):
    if f is None:
        return False
    try:
        index = f.__code__.co_freevars.index("__class__")
    except ValueError:
        return False
    cell = f.__closure__[index]
    try:
        contents = cell.cell_contents
    except ValueError:
        return False
    if contents is old:
        cell.cell_contents = new
        return True
    return False


def _by_dataclass_slots(cls):
    import dataclasses, inspect, types
    if "__slots__" in cls.__dict__:
        raise TypeError(f"{cls.__name__} already specifies __slots__")
    namespace = dict(cls.__dict__)
    field_names = tuple(f.name for f in dataclasses.fields(cls))
    inherited = set()
    for base in cls.__mro__[1:-1]:
        inherited.update(_by_dataclass_slots_of(base))
    namespace["__slots__"] = tuple(name for name in field_names if name not in inherited)
    for name in field_names:
        namespace.pop(name, None)
    namespace.pop("__dict__", None)
    namespace.pop("__weakref__", None)
    qualname = getattr(cls, "__qualname__", None)
    new = type(cls)(cls.__name__, cls.__bases__, namespace)
    if qualname is not None:
        new.__qualname__ = qualname
    if cls.__dataclass_params__.frozen:
        if "__getstate__" not in namespace:
            new.__getstate__ = _by_dataclass_getstate
        if "__setstate__" not in namespace:
            new.__setstate__ = _by_dataclass_setstate
    for member in new.__dict__.values():
        # a static or class method keeps its function where 3.10 and later let
        # `unwrap` find it
        if isinstance(member, (staticmethod, classmethod)):
            member = member.__func__
        member = inspect.unwrap(member)
        if isinstance(member, types.FunctionType):
            if _by_dataclass_class_cell(member, cls, new):
                break
        elif isinstance(member, property):
            if (
                _by_dataclass_class_cell(member.fget, cls, new)
                or _by_dataclass_class_cell(member.fset, cls, new)
                or _by_dataclass_class_cell(member.fdel, cls, new)
            ):
                break
    return new
