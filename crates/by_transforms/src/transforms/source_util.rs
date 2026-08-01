use std::fmt::Display;

use ruff_python_ast::helpers::consumed_keywords;
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Decorator, Expr, Parameters, Stmt, StmtImportFrom};
use ruff_text_size::{Ranged, TextRange, TextSize};

/// Names a temporary a lowering needs in its output.
///
/// The name is a dunder because a lowering fires wherever its construct was
/// written, and a class body is one of those places. Python's `enum` turns
/// every ordinary name assigned in a class body into a member, and `EnumDict`
/// records the name on assignment, so a later `del` cannot take it back — the
/// name has to be one `enum` never records. A dunder is machinery to `enum` as
/// much as it is to us, and it is not name-mangled, because mangling wants at
/// most one *trailing* underscore.
///
/// The parser's destructuring binder is a dunder for the same reason — see
/// [`destructure_binder_name`](ruff_python_ast::destructure_binder_name).
pub(crate) fn temporary_name(kind: &str, index: impl Display) -> String {
    format!("__by_{kind}_{index}__")
}

/// Byte offset of the start of the line containing `pos`. Lines begin at
/// either offset 0 or one byte past the previous `\n`
pub(crate) fn line_start(source: &str, pos: TextSize) -> TextSize {
    let offset = usize::from(pos);
    let start = source[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    TextSize::try_from(start).expect("line start fits u32")
}

/// Leading-whitespace slice of the line containing `pos`. Empty when the line
/// has no indentation or `pos` falls inside the indentation prefix
pub(crate) fn line_indent(source: &str, pos: TextSize) -> &str {
    let line_start = usize::from(line_start(source, pos));
    let offset = usize::from(pos);
    let rest = &source[line_start..offset];
    let ws_len = rest.len() - rest.trim_start().len();
    &source[line_start..line_start + ws_len]
}

/// Source range of the keyword that separates a `from` import's module from its
/// imported names — `import`, or basedpython's `export`.
///
/// The scan starts past the module name so a module *called* `import` or
/// `export` can't be mistaken for the keyword; when the module is omitted
/// (`from . export y`) it starts at `from`, which is neither word
pub(crate) fn from_import_keyword_range(
    source: &str,
    import: &StmtImportFrom,
) -> Option<TextRange> {
    let keywords: &[&str] = if import.is_export {
        &["export"]
    } else {
        &["import"]
    };
    let start = import
        .module
        .as_ref()
        .map_or_else(|| import.start(), Ranged::end);
    let end = import
        .names
        .first()
        .map_or_else(|| import.end(), Ranged::start);
    consumed_keywords(source, TextRange::new(start, end), keywords).next()
}

/// True when `dec` is a synthetic decorator emitted by the parser for a
/// basedpython modifier keyword (e.g. `let`, `final`, `abstract`,
/// `decorator_keyword`) rather than a user-written `@…`. Synthetic nodes
/// have no `@` byte at their range start in the source
pub(crate) fn is_synthetic_decorator(source: &str, dec: &Decorator) -> bool {
    let start = usize::from(dec.range().start());
    source.as_bytes().get(start).copied() != Some(b'@')
}

/// Invoke `on_ann` on every annotation expression reachable from `stmt`.
/// Covers `AnnAssign` targets, `TypeAlias` RHS, function parameter
/// annotations (regular, vararg, kwarg), return annotations, and recurses
/// into nested function bodies. Used by the reverse transforms (callable,
/// `not_type`, `tuple_type`, intersection) to share annotation-site discovery
pub(crate) fn for_each_annotation_in_stmt<F: FnMut(&Expr)>(stmt: &Stmt, mut on_ann: F) {
    let mut walker = AnnotationWalker {
        on_ann: &mut on_ann,
    };
    walker.visit_stmt(stmt);
}

struct AnnotationWalker<'f, F: FnMut(&Expr)> {
    on_ann: &'f mut F,
}

impl<F: FnMut(&Expr)> AnnotationWalker<'_, F> {
    fn walk_parameters(&mut self, params: &Parameters) {
        for p in params.iter_non_variadic_params() {
            if let Some(ann) = &p.parameter.annotation {
                (self.on_ann)(ann);
            }
        }
        if let Some(v) = &params.vararg
            && let Some(ann) = &v.annotation
        {
            (self.on_ann)(ann);
        }
        if let Some(k) = &params.kwarg
            && let Some(ann) = &k.annotation
        {
            (self.on_ann)(ann);
        }
    }
}

impl<'ast, F: FnMut(&Expr)> Visitor<'ast> for AnnotationWalker<'_, F> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::AnnAssign(a) => (self.on_ann)(&a.annotation),
            Stmt::TypeAlias(a) => (self.on_ann)(&a.value),
            Stmt::FunctionDef(f) => {
                self.walk_parameters(&f.parameters);
                if let Some(ret) = &f.returns {
                    (self.on_ann)(ret);
                }
                for s in &f.body {
                    self.visit_stmt(s);
                }
            }
            _ => walk_stmt(self, stmt),
        }
    }
}

/// Substitute every whole-word occurrence of a key in `renames` with its value.
///
/// Operates on rendered text rather than the AST because the callers re-emit a
/// type expression into synthesized output — a hoisted class body — where the
/// PEP 695 polyfill's mangled typevar names are what resolve.
pub(crate) fn rename_identifiers(
    text: &str,
    renames: &std::collections::HashMap<&str, &str>,
) -> String {
    if renames.is_empty() {
        return text.to_owned();
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &text[start..i];
            out.push_str(renames.get(ident).copied().unwrap_or(ident));
        } else {
            out.push(c as char);
            i += 1;
        }
    }
    out
}
