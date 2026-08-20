//! Marks every `import` and `from import` statement as lazy.
//!
//! Two emission strategies, chosen by target Python version:
//!   - **`min_version >= 3.15`** — prepend the `lazy` keyword (PEP 810)
//!   - **`min_version < 3.15`** — rewrite the statement to call a runtime
//!     polyfill (`_lazy_module` for module imports, `_lazy_attr` for `from`
//!     imports). The polyfill defines helpers in the preamble that wrap
//!     `importlib.util.LazyLoader` and a small proxy class
//!
//! Both modes skip:
//!   - `from __future__ import ...` — compiler directive
//!   - `from x import *` — `lazy` is not allowed with star imports
//!   - `TYPE_CHECKING` — a flag read statically, whose name may only ever be
//!     bound to `False`
//!
//! The polyfill additionally skips forms it can't safely rewrite:
//!   - relative imports (`from .pkg import x`)
//!   - `import a.b` without an alias (binds the top package, which
//!     `LazyLoader` does not register)
//!   - bootstrap modules (`sys`, `importlib*`) — the helpers depend on them
//!
//! A multi-name `import a, b` mixing the two is split, keeping a plain import
//! for the names that stay eager.

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Stmt, StmtImport, StmtImportFrom};
use ruff_text_size::{Ranged, TextRange, TextSize};

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent lazy-import state flags, not a state machine"
)]
pub(crate) struct LazyImport<'src> {
    source: &'src str,
    /// whether the walk is currently in module-level statements. PEP 810's
    /// `lazy` keyword is only valid there
    at_module_level: bool,
    /// modules whose import must stay eager whatever the target version,
    /// because executing them is the point: a module declaring a conformance
    /// registers it at import, so deferring the import defers the conformance
    /// out of existence
    eager: Vec<String>,
    /// True when the target Python version supports PEP 810 (3.15+). When
    /// false, the transform uses the runtime polyfill instead
    keyword_supported: bool,
    pub(crate) edits: Vec<Fix>,
    /// True when at least one statement was rewritten to call
    /// `_lazy_module`; the preamble must define the module helper
    pub(crate) needs_module_helper: bool,
    /// True when at least one statement was rewritten to call `_lazy_attr`;
    /// the preamble must define the `_LazyAttr` proxy (and `_lazy_module`,
    /// which `_lazy_attr` calls)
    pub(crate) needs_attr_helper: bool,
    /// True when at least one `from ty_extensions import X` was rewritten to
    /// a `_TyExtMarker` assignment; the preamble must define the marker
    pub(crate) needs_ty_ext_marker: bool,
    /// True when `Character` was imported from `ty_extensions`. unlike the
    /// other type-only names, `Character` is a *concrete* runtime class
    /// (`class Character(str)`) that the grapheme accessors construct, so the
    /// preamble must define the real class rather than the marker
    pub(crate) needs_character_class: bool,
}

impl<'src> LazyImport<'src> {
    pub(crate) fn new(source: &'src str, keyword_supported: bool, eager: &[String]) -> Self {
        Self {
            source,
            at_module_level: true,
            eager: eager.to_vec(),
            keyword_supported,
            edits: Vec::new(),
            needs_module_helper: false,
            needs_attr_helper: false,
            needs_ty_ext_marker: false,
            needs_character_class: false,
        }
    }

    /// Strip the leading `lazy` keyword and any trailing whitespace from a
    /// statement. Called when the statement falls into a skipped category
    /// (star, `__future__`, polyfill-unsafe form) but the parser saw `lazy`
    fn strip_lazy_keyword(&mut self, stmt_range: TextRange) {
        let start = stmt_range.start();
        let text = &self.source[usize::from(start)..usize::from(stmt_range.end())];
        let mut drop_len = 0usize;
        loop {
            let rest = &text[drop_len..];
            let Some(after_kw) = rest.strip_prefix("lazy") else {
                break;
            };
            // require a word boundary so we don't eat "lazyfoo"
            let next = after_kw.chars().next();
            if matches!(next, Some(c) if !c.is_whitespace()) {
                break;
            }
            let ws_len = after_kw.len() - after_kw.trim_start_matches([' ', '\t']).len();
            drop_len += "lazy".len() + ws_len;
        }
        if drop_len == 0 {
            return;
        }
        let strip_end = start + TextSize::try_from(drop_len).unwrap();
        self.edits
            .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                start, strip_end,
            ))));
    }

    fn insert_lazy_keyword(&mut self, at: TextSize) {
        self.edits
            .push(Fix::safe_edit(Edit::insertion("lazy ".to_owned(), at)));
    }

    fn line_indent(&self, range: TextRange) -> &str {
        let stmt_start = usize::from(range.start());
        let line_start = self.source[..stmt_start]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        &self.source[line_start..stmt_start]
    }

    /// a module whose import must stay eager whatever the target version: its
    /// execution is the point of the import, not just the binding
    fn is_eager(&self, name: &str) -> bool {
        self.eager.iter().any(|module| module == name)
    }

    fn is_bootstrap(name: &str) -> bool {
        matches!(name, "sys" | "importlib") || name.starts_with("importlib.")
    }

    fn process_import(&mut self, node: &StmtImport) {
        // a module whose execution is the point of the import is never deferred,
        // whichever mechanism this target uses
        if node
            .names
            .iter()
            .any(|alias| self.is_eager(alias.name.id.as_str()))
        {
            if node.is_lazy {
                self.strip_lazy_keyword(node.range());
            }
            return;
        }
        if self.keyword_supported {
            if !node.is_lazy {
                self.insert_lazy_keyword(node.range().start());
            }
            return;
        }
        let mut lines: Vec<String> = Vec::new();
        // aliases that must stay eager, rendered back as they were written. the
        // rewrite replaces the whole statement, so anything not lazified here
        // has to be re-emitted as a plain import or its name is simply gone
        let mut stays_eager: Vec<String> = Vec::new();
        for alias in &node.names {
            let module = alias.name.id.as_str();
            // `import a.b` without `as` binds `a`, not `a.b`, so `LazyLoader`
            // on `a.b` would never trigger the lazy binding
            let unlazifiable =
                Self::is_bootstrap(module) || (alias.asname.is_none() && module.contains('.'));
            if unlazifiable {
                stays_eager.push(match &alias.asname {
                    Some(a) => format!("{module} as {}", a.id),
                    None => module.to_owned(),
                });
                continue;
            }
            let bind = match &alias.asname {
                Some(a) => a.id.as_str(),
                None => module,
            };
            self.needs_module_helper = true;
            lines.push(format!("{bind} = _lazy_module(\"{module}\")"));
        }
        if lines.is_empty() {
            // Every alias was skipped — strip any `lazy` keyword the parser
            // saw so the output stays valid Python
            if node.is_lazy {
                self.strip_lazy_keyword(node.range());
            }
            return;
        }
        if !stays_eager.is_empty() {
            // a statement mixing lazifiable modules with unlazifiable ones is
            // split rather than rewritten wholesale, e.g. `import math, sys`
            // becomes `import sys` plus a `_lazy_module("math")` binding
            lines.insert(0, format!("import {}", stays_eager.join(", ")));
        }
        let indent = self.line_indent(node.range());
        let separator = format!("\n{indent}");
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            lines.join(&separator),
            node.range(),
        )));
    }

    fn process_from(&mut self, node: &StmtImportFrom) {
        let is_future = node
            .module
            .as_ref()
            .is_some_and(|m| m.id.as_str() == "__future__");
        let is_star = node.names.iter().any(|a| a.name.id.as_str() == "*");
        // `TYPE_CHECKING` is a flag a checker reads statically, not an ordinary
        // binding: deferring it rebinds the name to a proxy, and the name may
        // only ever be bound to `False`
        let binds_type_checking = node
            .names
            .iter()
            .any(|a| a.asname.is_none() && a.name.id.as_str() == "TYPE_CHECKING");
        if binds_type_checking {
            if node.is_lazy {
                self.strip_lazy_keyword(node.range());
            }
            return;
        }

        // a module whose execution is the point of the import is never deferred
        if node
            .module
            .as_ref()
            .is_some_and(|module| self.is_eager(module.id.as_str()))
        {
            if node.is_lazy {
                self.strip_lazy_keyword(node.range());
            }
            return;
        }

        if self.keyword_supported {
            if is_future || is_star {
                if node.is_lazy {
                    self.strip_lazy_keyword(node.range());
                }
                return;
            }
            if !node.is_lazy {
                self.insert_lazy_keyword(node.range().start());
            }
            return;
        }

        // Polyfill mode for `from x import y`. Relative imports use
        // `importlib.util.resolve_name(..., __package__)` at runtime
        let polyfill_skip = is_future
            || is_star
            || (node.level == 0
                && node
                    .module
                    .as_ref()
                    .is_some_and(|m| Self::is_bootstrap(m.id.as_str())))
            || (node.level == 0 && node.module.is_none());
        if polyfill_skip {
            if node.is_lazy {
                self.strip_lazy_keyword(node.range());
            }
            return;
        }

        let module_part = node.module.as_ref().map(|m| m.id.as_str()).unwrap_or("");
        let dots: String = ".".repeat(node.level as usize);
        let is_relative = node.level > 0;
        // `ty_extensions` is a ty-only module — it has no runtime existence on
        // PyPI. Names imported from it (`Intersection`, `Not`, `TypeOf`,
        // `Top`) are type-only markers. Replace with a stub class that supports
        // `X[T]`, `X | Y`, and use-as-base.
        //
        // `JustFloat` / `JustComplex` are the exception: they mean *just* the
        // builtin (basedpython's int-excluding `float` / `complex`), and that
        // exclusion is static-only — at runtime they are the builtins. Binding
        // them to the marker breaks any consumer that evaluates the annotation
        // at runtime (`get_type_hints`, and so pydantic / dataclasses schema
        // generation), so bind them to the builtin instead
        let is_ty_ext = !is_relative && module_part == "ty_extensions";
        let mut lines: Vec<String> = Vec::new();
        // whether the statement imported a bare `Character` that contributed no
        // binding line — the import must still be *removed* (the preamble class
        // defines `Character`), so force an empty replacement below
        let mut character_only = false;
        for alias in &node.names {
            let name = alias.name.id.as_str();
            let bind = alias.asname.as_ref().map(|a| a.id.as_str()).unwrap_or(name);
            if is_ty_ext {
                // `Character` is a concrete runtime class, not a type-only
                // marker — the preamble defines `class Character(str)`. a bare
                // `Character` import needs no binding line (the class *is* the
                // binding); an aliased one binds the alias to the class
                if name == "Character" {
                    self.needs_character_class = true;
                    if bind != "Character" {
                        lines.push(format!("{bind} = Character"));
                    } else {
                        character_only = true;
                    }
                    continue;
                }
                // `JustFloat` / `JustComplex` are type-only aliases whose
                // runtime value is the builtin, so `get_type_hints` (and thus
                // pydantic / dataclasses schema generation) resolves them
                match name {
                    "JustFloat" => lines.push(format!("{bind} = float")),
                    "JustComplex" => lines.push(format!("{bind} = complex")),
                    _ => {
                        self.needs_ty_ext_marker = true;
                        lines.push(format!("{bind} = _TyExtMarker"));
                    }
                }
                continue;
            }
            if is_relative && module_part.is_empty() {
                // `from . import x` — `x` is a submodule of the current
                // package. Resolve the relative target at runtime
                self.needs_module_helper = true;
                let rel = format!("{dots}{name}");
                lines.push(format!(
                    "{bind} = _lazy_module(_by_iu.resolve_name(\"{rel}\", __package__))"
                ));
            } else if is_relative {
                // `from .pkg import x` — lazy attribute on the resolved
                // parent, matching the `from pkg import x` shape
                self.needs_attr_helper = true;
                let rel = format!("{dots}{module_part}");
                lines.push(format!(
                    "{bind} = _lazy_attr(_by_iu.resolve_name(\"{rel}\", __package__), \"{name}\")"
                ));
            } else {
                self.needs_attr_helper = true;
                lines.push(format!(
                    "{bind} = _lazy_attr(\"{module_part}\", \"{name}\")"
                ));
            }
        }
        // `character_only` means a bare `Character` import produced no lines but
        // must still be dropped (the preamble class defines `Character`) so it
        // doesn't survive as a runtime `from ty_extensions import Character`
        if lines.is_empty() {
            if character_only {
                self.edits.push(Fix::safe_edit(Edit::deletion(
                    node.range().start(),
                    node.range().end(),
                )));
            }
            return;
        }
        let indent = self.line_indent(node.range());
        let separator = format!("\n{indent}");
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            lines.join(&separator),
            node.range(),
        )));
    }
}

/// whether `test` is the `TYPE_CHECKING` flag, spelled bare or through the module it
/// comes from
fn is_type_checking_test(test: &ruff_python_ast::Expr) -> bool {
    match test {
        ruff_python_ast::Expr::Name(name) => name.id.as_str() == "TYPE_CHECKING",
        ruff_python_ast::Expr::Attribute(attr) => attr.attr.as_str() == "TYPE_CHECKING",
        _ => false,
    }
}

impl<'ast> Visitor<'ast> for LazyImport<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            // PEP 810 allows `lazy` only on a module-level import: inside a
            // function, a class body, or a `try`, it is a syntax error. the
            // polyfill's rewrite is legal anywhere, but it defers module
            // *execution*, which an import written inside a function has
            // usually been placed there to control — so both modes leave a
            // nested import exactly as written
            Stmt::Import(n) if self.at_module_level => self.process_import(n),
            Stmt::ImportFrom(n) if self.at_module_level => self.process_from(n),
            Stmt::Import(n) => {
                if n.is_lazy {
                    self.strip_lazy_keyword(n.range());
                }
            }
            Stmt::ImportFrom(n) => {
                if n.is_lazy {
                    self.strip_lazy_keyword(n.range());
                }
            }
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) | Stmt::Try(_) => {
                let outer = std::mem::replace(&mut self.at_module_level, false);
                walk_stmt(self, stmt);
                self.at_module_level = outer;
            }
            // an `if TYPE_CHECKING:` body never runs, so an import inside it has
            // no execution to defer — and deferring it would rebind the name to
            // a proxy, which is the one thing a type expression cannot read
            Stmt::If(node) if is_type_checking_test(&node.test) => {
                let outer = std::mem::replace(&mut self.at_module_level, false);
                walk_stmt(self, stmt);
                self.at_module_level = outer;
            }
            _ => walk_stmt(self, stmt),
        }
    }
}

/// `from x import y` proxy for the polyfill mode (python < 3.15, which has no
/// PEP 810 `lazy` keyword). deferring the *attribute read* is what needs a
/// proxy at all: `_lazy_module` already defers the module's execution, but
/// reading `y` off it would force that execution immediately.
///
/// the proxy must be *transparent*: python looks special methods up on the
/// type, never through `__getattr__`, so a dunder that isn't forwarded here
/// silently falls back to `object`'s version. that is not a missing feature
/// but a correctness bug — an unforwarded `__eq__` makes `a == b` compare
/// proxy identity and answer `False` for equal values. the forwarding table
/// below is generated in a loop rather than hand-written so the set stays
/// auditable, and each operator is applied to the *resolved* value so
/// python's full binary-op protocol (reflected operands, `NotImplemented`)
/// still runs.
///
/// `__class__` makes `isinstance(proxy, C)` work, and `__instancecheck__`
/// makes `isinstance(x, proxy)` work for a lazily-imported class (`isinstance`
/// looks `__instancecheck__` up on `type(classinfo)`, which is `_LazyAttr`).
/// `type(proxy)` and `proxy is x` cannot be fixed by any proxy — that is
/// exactly why PEP 810 is a language feature — and are documented limits of
/// this polyfill
const LAZY_ATTR_PROXY: &str = r#"class _LazyAttr:
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
def _by_lazy_forward():
    import operator as op
    def one(f): return lambda s: f(s._by_resolve())
    def two(f): return lambda s, o: f(s._by_resolve(), o)
    def rtwo(f): return lambda s, o: f(o, s._by_resolve())
    for n, f in (("add", op.add), ("sub", op.sub), ("mul", op.mul), ("matmul", op.matmul),
                 ("truediv", op.truediv), ("floordiv", op.floordiv), ("mod", op.mod),
                 ("divmod", divmod), ("pow", op.pow), ("lshift", op.lshift),
                 ("rshift", op.rshift), ("and", op.and_), ("xor", op.xor), ("or", op.or_)):
        setattr(_LazyAttr, "__" + n + "__", two(f))
        setattr(_LazyAttr, "__r" + n + "__", rtwo(f))
    for n in ("lt", "le", "eq", "ne", "gt", "ge"):
        setattr(_LazyAttr, "__" + n + "__", two(getattr(op, n)))
    for n, f in (("neg", op.neg), ("pos", op.pos), ("abs", abs), ("invert", op.inv),
                 ("len", len), ("iter", iter), ("next", next), ("bool", bool),
                 ("str", str), ("repr", repr), ("bytes", bytes), ("int", int),
                 ("float", float), ("complex", complex), ("index", op.index),
                 ("hash", hash), ("reversed", reversed)):
        setattr(_LazyAttr, "__" + n + "__", one(f))
    setattr(_LazyAttr, "__getitem__", two(op.getitem))
    setattr(_LazyAttr, "__contains__", two(op.contains))
    setattr(_LazyAttr, "__delitem__", two(op.delitem))
    setattr(_LazyAttr, "__setitem__", lambda s, k, v: op.setitem(s._by_resolve(), k, v))
    setattr(_LazyAttr, "__format__", lambda s, f: format(s._by_resolve(), f))
    setattr(_LazyAttr, "__round__", lambda s, *a: round(s._by_resolve(), *a))
    setattr(_LazyAttr, "__enter__", lambda s: s._by_resolve().__enter__())
    setattr(_LazyAttr, "__exit__", lambda s, *a: s._by_resolve().__exit__(*a))
_by_lazy_forward()
def _lazy_attr(mod, attr): return _LazyAttr(mod, attr)
"#;

/// `Character` — a concrete `str` subclass, so the grapheme accessors can
/// construct genuine instances and `isinstance(x, Character)` works.
///
/// it is interned in a `sys.modules` registry rather than defined per file
/// because class *identity* is what `isinstance` tests: a plain
/// `class Character(str)` in each transpiled module would make every module's
/// `Character` a distinct class object, so a value built in one module would
/// fail `isinstance(v, Character)` in another. the registry gives every module
/// in a process the one class — first definer wins, the rest reuse it.
///
/// this keeps transpiled output self-contained, which a shared import would
/// not: `x: Character = "a"` currently needs nothing installed. if `Character`
/// ever moves into a shipped `basedpython.by`, it becomes an ordinary import
/// from that module and this registry goes away
const CHARACTER_CLASS: &str = r#"import types as _by_types
_by_rt = _by_sys.modules.setdefault("_by_runtime", _by_types.ModuleType("_by_runtime"))
if not hasattr(_by_rt, "Character"):
    class Character(str):
        __slots__ = ()
    _by_rt.Character = Character
Character = _by_rt.Character
"#;

/// Preamble snippet defining the runtime helpers used by polyfill-mode
/// lazified imports. Emitted once per file when any lazification fires
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "independent which-helpers-to-emit flags, not a state machine"
)]
pub(crate) fn polyfill_preamble(
    needs_module: bool,
    needs_attr: bool,
    needs_ty_ext: bool,
    needs_character_class: bool,
) -> String {
    if !needs_module && !needs_attr && !needs_ty_ext && !needs_character_class {
        return String::new();
    }
    let needs_module = needs_module || needs_attr;
    let mut out = String::new();
    if needs_module {
        out.push_str("import importlib as _by_il, importlib.util as _by_iu, sys as _by_sys\n");
        out.push_str("def _lazy_module(name):\n");
        out.push_str("    mod = _by_sys.modules.get(name)\n");
        out.push_str("    if mod is not None:\n");
        out.push_str("        return mod\n");
        // a dotted submodule can't take the `LazyLoader` path: `find_spec`
        // needs the parent package imported, and a frozen alias like
        // `collections.abc` (-> `_collections_abc`) fails to re-execute under a
        // lazy load. import it eagerly — laziness is still preserved at the
        // attribute level, since `_LazyAttr` only calls this on first use
        out.push_str("    if \".\" in name:\n");
        out.push_str("        return _by_il.import_module(name)\n");
        out.push_str("    spec = _by_iu.find_spec(name)\n");
        // `find_spec` returns `None` when the module isn't installed; raise
        // a clean `ImportError` instead of letting the next line crash with
        // `AttributeError: 'NoneType' object has no attribute 'loader'`
        out.push_str("    if spec is None or spec.loader is None:\n");
        out.push_str("        raise ImportError(f\"No module named {name!r}\", name=name)\n");
        out.push_str("    spec.loader = _by_iu.LazyLoader(spec.loader)\n");
        out.push_str("    mod = _by_iu.module_from_spec(spec)\n");
        // publishing before `exec_module` leaves a module nothing has executed
        // in `sys.modules`, and `exec_module` is not quiet: it opens with
        // `import threading`, which through 3.12 reached `functools` and its
        // `from collections import namedtuple`. lazifying `collections` then
        // handed that shell back and the import failed. which stdlib module
        // sits in the window is an accident of the version, so the name is
        // claimed only once the module really is lazy, and `setdefault` yields
        // to whoever imported it for real while the window was open
        out.push_str("    spec.loader.exec_module(mod)\n");
        out.push_str("    return _by_sys.modules.setdefault(name, mod)\n");
    } else if needs_character_class {
        // the `Character` registry reads `sys.modules`, so `sys` must be bound
        // even when no import was lazified
        out.push_str("import sys as _by_sys\n");
    }
    if needs_attr {
        out.push_str(LAZY_ATTR_PROXY);
    }
    if needs_ty_ext {
        // type-only marker for `ty_extensions` imports. Supports the type
        // expression operations the language allows on these names without
        // attempting a (non-existent) runtime import
        out.push_str("class _TyExtMarker:\n");
        out.push_str("    def __class_getitem__(cls, k): return cls\n");
    }
    if needs_character_class {
        out.push_str(CHARACTER_CLASS);
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::config::PythonVersion;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn cfg_315() -> Config {
        Config {
            min_version: PythonVersion::from((3, 15)),
            ..Config {
                lazy_imports: true,
                ..Config::test_default()
            }
        }
    }

    fn check(input: &str, expected: &str) {
        assert_eq!(transpile(input, &cfg_315()).unwrap(), expected);
    }

    #[test]
    fn keyword_simple_import() {
        check("import os\n", "lazy import os\n");
    }

    #[test]
    fn keyword_import_as() {
        check("import os as o\n", "lazy import os as o\n");
    }

    #[test]
    fn keyword_from_import() {
        check("from os import path\n", "lazy from os import path\n");
    }

    #[test]
    fn keyword_future_unchanged() {
        check(
            "from __future__ import annotations\n",
            "from __future__ import annotations\n",
        );
    }

    #[test]
    fn keyword_star_unchanged() {
        check("from os import *\n", "from os import *\n");
    }

    #[test]
    fn keyword_type_checking_unchanged() {
        // deferring the flag would rebind the name to a proxy; a checker only
        // ever accepts it bound to `False`
        check(
            "from typing import TYPE_CHECKING\n",
            "from typing import TYPE_CHECKING\n",
        );
    }

    #[test]
    fn keyword_relative_lazified() {
        check("from .pkg import x\n", "lazy from .pkg import x\n");
    }

    #[test]
    fn keyword_existing_passes_through() {
        check("lazy import os\n", "lazy import os\n");
    }

    #[test]
    fn keyword_stripped_on_future() {
        check(
            "lazy from __future__ import annotations\n",
            "from __future__ import annotations\n",
        );
    }

    #[test]
    fn keyword_stripped_on_star() {
        check("lazy from os import *\n", "from os import *\n");
    }

    #[test]
    fn keyword_nested_indent_preserved() {
        check(
            indoc! {"
                if True:
                    import os
            "},
            indoc! {"
                if True:
                    lazy import os
            "},
        );
    }

    // ---- polyfill mode (default config, 3.10) ----

    fn check_polyfill_body(input: &str, expected_body: &str) {
        let out = transpile(
            input,
            &Config {
                lazy_imports: true,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains("def _lazy_module(name):"),
            "missing _lazy_module helper in:\n{out}"
        );
        assert!(
            out.ends_with(expected_body),
            "expected suffix:\n{expected_body}\n---got---\n{out}"
        );
    }

    #[test]
    fn polyfill_simple_import() {
        check_polyfill_body("import other\n", "other = _lazy_module(\"other\")\n");
    }

    #[test]
    fn polyfill_import_as() {
        check_polyfill_body("import os as o\n", "o = _lazy_module(\"os\")\n");
    }

    #[test]
    fn polyfill_type_checking_stays_eager() {
        check_polyfill_body(
            "from typing import TYPE_CHECKING\nimport other\n",
            "from typing import TYPE_CHECKING\nother = _lazy_module(\"other\")\n",
        );
    }

    #[test]
    fn attr_proxy_is_isinstance_transparent() {
        // other lowerings (soundness checks, checked casts) pass imported
        // names as runtime types; the proxy must delegate isinstance/issubclass
        let out = transpile(
            "from os.path import basename\n",
            &Config {
                lazy_imports: true,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains(
                "def __instancecheck__(self, o): return isinstance(o, self._by_resolve())"
            ),
            "proxy must delegate __instancecheck__ in:\n{out}"
        );
        assert!(
            out.contains(
                "def __subclasscheck__(self, o): return issubclass(o, self._by_resolve())"
            ),
            "proxy must delegate __subclasscheck__ in:\n{out}"
        );
    }

    #[test]
    fn polyfill_dotted_with_alias() {
        check_polyfill_body("import os.path as p\n", "p = _lazy_module(\"os.path\")\n");
    }

    #[test]
    fn polyfill_dotted_no_alias_stays_eager() {
        // `import a.b` binds `a` — `LazyLoader` can't register `a` from `a.b`
        let out = transpile(
            "import os.path\n",
            &Config {
                lazy_imports: true,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert_eq!(out, "import os.path\n");
    }

    #[test]
    fn polyfill_from_import() {
        check_polyfill_body(
            "from os import path\n",
            "path = _lazy_attr(\"os\", \"path\")\n",
        );
    }

    #[test]
    fn polyfill_from_import_multiple() {
        check_polyfill_body(
            "from os import path, getcwd\n",
            indoc! {"
                path = _lazy_attr(\"os\", \"path\")
                getcwd = _lazy_attr(\"os\", \"getcwd\")
            "},
        );
    }

    fn transpile_polyfill(input: &str) -> String {
        transpile(
            input,
            &Config {
                lazy_imports: true,
                ..Config::test_default()
            },
        )
        .unwrap()
    }

    #[test]
    fn ty_ext_just_float_binds_to_builtin() {
        // `float` lowers to `JustFloat`; its runtime binding must be the
        // builtin so annotation-introspecting consumers (`get_type_hints`,
        // pydantic / dataclasses schema generation) see a real type, not the
        // opaque `_TyExtMarker`
        let out = transpile_polyfill("a: float\n");
        assert!(
            out.contains("JustFloat = float"),
            "JustFloat should bind to the builtin, got:\n{out}"
        );
        assert!(
            !out.contains("JustFloat = _TyExtMarker"),
            "JustFloat must not bind to the marker, got:\n{out}"
        );
    }

    #[test]
    fn ty_ext_just_complex_binds_to_builtin() {
        let out = transpile_polyfill("a: complex\n");
        assert!(
            out.contains("JustComplex = complex"),
            "JustComplex should bind to the builtin, got:\n{out}"
        );
    }

    #[test]
    fn ty_ext_other_markers_stay_marker() {
        // a genuinely type-only marker (`Not`, from `not int`) has no runtime
        // meaning, so it keeps the opaque `_TyExtMarker` binding
        let out = transpile_polyfill("a: not int\n");
        assert!(
            out.contains("Not = _TyExtMarker"),
            "non-Just markers should keep the marker binding, got:\n{out}"
        );
    }

    #[test]
    fn polyfill_relative_submodule() {
        check_polyfill_body(
            "from . import x\n",
            "x = _lazy_module(_by_iu.resolve_name(\".x\", __package__))\n",
        );
    }

    #[test]
    fn polyfill_relative_attr() {
        check_polyfill_body(
            "from .pkg import x\n",
            "x = _lazy_attr(_by_iu.resolve_name(\".pkg\", __package__), \"x\")\n",
        );
    }

    #[test]
    fn polyfill_relative_double_dot() {
        check_polyfill_body(
            "from .. import x\n",
            "x = _lazy_module(_by_iu.resolve_name(\"..x\", __package__))\n",
        );
    }

    #[test]
    fn polyfill_star_unchanged() {
        let out = transpile(
            "from os import *\n",
            &Config {
                lazy_imports: true,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert_eq!(out, "from os import *\n");
    }

    #[test]
    fn polyfill_future_unchanged() {
        let out = transpile(
            "from __future__ import annotations\n",
            &Config {
                lazy_imports: true,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert_eq!(out, "from __future__ import annotations\n");
    }

    #[test]
    fn polyfill_bootstrap_sys_unchanged() {
        let out = transpile(
            "import sys\n",
            &Config {
                lazy_imports: true,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert_eq!(out, "import sys\n");
    }

    #[test]
    fn polyfill_multi_name_splits_around_bootstrap() {
        // the rewrite replaces the whole statement, so `sys` has to come back
        // as a plain import — dropping it would leave the name unbound
        check_polyfill_body(
            "import math, sys, time\n",
            indoc! {"
                import sys
                math = _lazy_module(\"math\")
                time = _lazy_module(\"time\")
            "},
        );
    }

    #[test]
    fn polyfill_multi_name_splits_around_dotted() {
        check_polyfill_body(
            "import os.path, json\n",
            indoc! {"
                import os.path
                json = _lazy_module(\"json\")
            "},
        );
    }

    #[test]
    fn polyfill_multi_name_keeps_eager_alias() {
        check_polyfill_body(
            "import sys as system, json\n",
            indoc! {"
                import sys as system
                json = _lazy_module(\"json\")
            "},
        );
    }

    #[test]
    fn polyfill_multi_name_all_unlazifiable_stays_written() {
        // nothing to lazify, so the statement is left exactly as it was rather
        // than being reconstructed
        let out = transpile_polyfill("import sys, os.path\n");
        assert_eq!(out, "import sys, os.path\n");
    }

    #[test]
    fn polyfill_multi_name_nested_indent_preserved() {
        check_polyfill_body(
            indoc! {"
                if True:
                    import math, sys
            "},
            indoc! {"
                if True:
                    import sys
                    math = _lazy_module(\"math\")
            "},
        );
    }

    #[test]
    fn polyfill_lazy_keyword_lazifies() {
        // `lazy import os` on default config: keyword stripped, polyfill applied
        check_polyfill_body("lazy import os\n", "os = _lazy_module(\"os\")\n");
    }

    #[test]
    fn passthrough_in_python_mode() {
        let py = transpile(
            "import os\n",
            &Config {
                is_python: true,
                ..Config {
                    lazy_imports: true,
                    ..Config::test_default()
                }
            },
        )
        .unwrap();
        assert_eq!(py, "import os\n");
    }
}
