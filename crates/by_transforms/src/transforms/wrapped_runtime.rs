//! Shared runtime polyfills basedpython injects into the file that needs them.
//!
//! `Optional` is the runtime machine for wrapped optionals: it is both the
//! present-case value wrapper that `Some(x)` lowers to (`Optional(x)`, holding
//! `.value`) and the subscriptable type the `int??` annotation lowers to
//! (`Optional[int | None]`). Passes that emit either form inject this class via
//! [`PassContext::required_imports`](super::ast_driver::PassContext), which
//! dedupes identical entries so the class is defined at most once.
//!
//! `_by_discard` is the adapter a conversion site wraps a callable in when the
//! site asked for one returning `None`.

use ty_python_semantic::DISCARD_ADAPTER;

pub(crate) const OPTIONAL_RUNTIME: &str = "\
class Optional:
    def __init__(self, value):
        self.value = value

    def __class_getitem__(cls, item):
        return cls

    def __repr__(self):
        return f\"Some({self.value!r})\"
";

/// The adapter a callable is wrapped in where the site declared one returning
/// `None` and the callable returns something else — basedpython's coercion to
/// `None`, which the checker resolves as a conversion route.
///
/// A bare closure would throw the result away just as well. It would also stop
/// comparing equal to the callable it wraps, and python deregisters callbacks by
/// value all the time — `observers.remove(cb)`, `atexit.unregister(cb)`,
/// `signal.disconnect(cb)`. Delegating `__eq__` and `__hash__` is what keeps a
/// wrapped callback removable; delegating everything else through `__getattr__`
/// is what keeps `cb.__name__` answering for a framework that reads it
pub(crate) fn discard_return_runtime() -> String {
    format!(
        "\
class {DISCARD_ADAPTER}:
    __slots__ = (\"__wrapped__\",)

    def __init__(self, fn):
        self.__wrapped__ = fn

    def __call__(self, *args, **kwargs):
        self.__wrapped__(*args, **kwargs)

    def __getattr__(self, name):
        if name == \"__wrapped__\":
            raise AttributeError(name)
        return getattr(self.__wrapped__, name)

    def __eq__(self, other):
        if isinstance(other, {DISCARD_ADAPTER}):
            other = other.__wrapped__
        return self.__wrapped__ == other

    def __hash__(self):
        return hash(self.__wrapped__)
"
    )
}
