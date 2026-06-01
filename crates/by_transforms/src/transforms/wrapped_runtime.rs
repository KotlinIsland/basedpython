//! Shared runtime polyfill for basedpython wrapped optionals.
//!
//! `Optional` is the runtime machine for wrapped optionals: it is both the
//! present-case value wrapper that `Some(x)` lowers to (`Optional(x)`, holding
//! `.value`) and the subscriptable type the `int??` annotation lowers to
//! (`Optional[int | None]`). Passes that emit either form inject this class via
//! [`PassContext::required_imports`](super::ast_driver::PassContext), which
//! dedupes identical entries so the class is defined at most once.

pub(crate) const OPTIONAL_RUNTIME: &str = "\
class Optional:
    def __init__(self, value):
        self.value = value

    def __class_getitem__(cls, item):
        return cls

    def __repr__(self):
        return f\"Some({self.value!r})\"
";
