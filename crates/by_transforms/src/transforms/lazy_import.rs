//! Marks every `import` and `from import` statement as lazy.
//!
//! Two emission strategies, chosen by target Python version:
//!   - **`min_version >= 3.15`** — prepend the `lazy` keyword (PEP 810)
//!   - **`min_version < 3.15`** — rewrite the statement to call a runtime
//!     polyfill (`_lazy_module` for module imports, `_lazy_attr` for `from`
//!     imports). The helpers wrap `importlib.util.LazyLoader` and a small proxy
//!     class, and live in [`crate::runtime`] with the rest of what the emitted
//!     python calls
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
//!
//! A stub defers nothing — see [`Deferral::Never`].

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Stmt, StmtImport, StmtImportFrom};
use ruff_text_size::{Ranged, TextRange, TextSize};

/// How a module-level import is deferred.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Deferral {
    /// the PEP 810 `lazy` keyword, which python 3.15 and later parse
    Keyword,
    /// a call into the runtime's polyfill, for every target before that
    Polyfill,
    /// not at all. a stub is read by a checker and never executed, so it has
    /// no execution to defer, and what a checker reads off each import is the
    /// binding it makes. every import stays as written, less any `lazy`
    Never,
}

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
    /// bindings that must be bound to the real object rather than to a proxy.
    /// cpython's `except` checks that what it catches is a class inheriting
    /// `BaseException` and never consults `__instancecheck__`, so an exception
    /// class reached through the proxy raises `TypeError` from the handler
    eager_names: Vec<String>,
    deferral: Deferral,
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
    pub(crate) fn new(
        source: &'src str,
        deferral: Deferral,
        eager: &[String],
        eager_names: &[String],
    ) -> Self {
        Self {
            source,
            at_module_level: true,
            eager: eager.to_vec(),
            eager_names: eager_names.to_vec(),
            deferral,
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
        // a stub defers nothing, and a module whose execution is the point of the
        // import is never deferred, whichever mechanism this target uses
        if self.deferral == Deferral::Never
            || node
                .names
                .iter()
                .any(|alias| self.is_eager(alias.name.id.as_str()))
        {
            if node.is_lazy {
                self.strip_lazy_keyword(node.range());
            }
            return;
        }
        if self.deferral == Deferral::Keyword {
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
        if self.deferral == Deferral::Never {
            if node.is_lazy {
                self.strip_lazy_keyword(node.range());
            }
            return;
        }
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

        if self.deferral == Deferral::Keyword {
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
            if self.eager_names.iter().any(|eager| eager == bind) {
                // the proxy cannot stand here, so re-emit the import as written.
                // ahead of the relative branches: a relative import is the
                // common shape inside a package, and its exceptions are caught
                // just the same
                let spelling = if bind == name {
                    name.to_owned()
                } else {
                    format!("{name} as {bind}")
                };
                lines.push(format!("from {dots}{module_part} import {spelling}"));
            } else if is_relative && module_part.is_empty() {
                // `from . import x` — `x` is a submodule of the current
                // package. Resolve the relative target at runtime
                self.needs_module_helper = true;
                let rel = format!("{dots}{name}");
                lines.push(format!("{bind} = _lazy_module(\"{rel}\", __package__)"));
            } else if is_relative {
                // `from .pkg import x` — lazy attribute on the resolved
                // parent, matching the `from pkg import x` shape
                self.needs_attr_helper = true;
                let rel = format!("{dots}{module_part}");
                lines.push(format!(
                    "{bind} = _lazy_attr(\"{rel}\", \"{name}\", __package__)"
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

/// The runtime helpers a polyfill-mode lazified module calls, by the name it
/// calls them under. What each of those needs in turn — the proxy class, the
/// `sys` binding, the operator forwarding — is settled in [`crate::runtime`],
/// off the calls in their own bodies.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "independent which-helpers-are-needed flags, not a state machine"
)]
pub(crate) fn polyfill_helpers(
    needs_module: bool,
    needs_attr: bool,
    needs_ty_ext: bool,
    needs_character_class: bool,
) -> Vec<crate::runtime::Helper> {
    let mut helpers = Vec::new();
    // each only where the emitted code names it: a `from` import calls
    // `_lazy_attr` alone, and what that calls in turn is the runtime's business
    if needs_module {
        helpers.push(crate::runtime::LAZY_MODULE);
    }
    if needs_attr {
        helpers.push(crate::runtime::LAZY_ATTR);
    }
    if needs_ty_ext {
        helpers.push(crate::runtime::TY_EXT_MARKER);
    }
    if needs_character_class {
        helpers.push(crate::runtime::CHARACTER);
    }
    helpers
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
            out.contains("def _lazy_module("),
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
            "x = _lazy_module(\".x\", __package__)\n",
        );
    }

    #[test]
    fn polyfill_relative_attr() {
        check_polyfill_body(
            "from .pkg import x\n",
            "x = _lazy_attr(\".pkg\", \"x\", __package__)\n",
        );
    }

    #[test]
    fn polyfill_relative_double_dot() {
        check_polyfill_body(
            "from .. import x\n",
            "x = _lazy_module(\"..x\", __package__)\n",
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
    fn polyfill_exception_class_stays_eager() {
        // cpython's `except` refuses anything that is not a real class
        // inheriting `BaseException`, so the proxy cannot stand for one
        let source = indoc! {"
            from json import JSONDecodeError

            def f() -> None:
                try:
                    pass
                except JSONDecodeError:
                    pass
        "};
        // nothing was deferred, so the proxy runtime is not emitted at all
        assert_eq!(transpile_polyfill(source), source);
    }

    #[test]
    fn polyfill_exception_class_beside_a_lazy_name() {
        // the statement is split: only the exception has to be bound for real
        let out = transpile_polyfill("from json import JSONDecodeError, dumps\n");
        assert!(
            out.ends_with(indoc! {"
                from json import JSONDecodeError
                dumps = _lazy_attr(\"json\", \"dumps\")
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn polyfill_aliased_exception_class_stays_eager() {
        let out = transpile_polyfill("from json import JSONDecodeError as JDE\n");
        assert!(
            out.ends_with("from json import JSONDecodeError as JDE\n"),
            "got:\n{out}"
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

    /// a stub is read by a checker and never executed. an import in one declares
    /// the binding it makes, where a deferred one would declare a call result
    #[test]
    fn a_stub_keeps_its_imports_as_written() {
        let source = indoc! {"
            import json
            from dataclasses import dataclass
            from ty_extensions import Intersection
            lazy import csv
        "};
        for min_version in [PythonVersion::PY310, PythonVersion::from((3, 15))] {
            let config = Config {
                is_stub: true,
                min_version,
                ..cfg_315()
            };
            assert_eq!(
                transpile(source, &config).unwrap(),
                indoc! {"
                    import json
                    from dataclasses import dataclass
                    from ty_extensions import Intersection
                    import csv
                "},
                "for {min_version}"
            );
        }
    }
}
