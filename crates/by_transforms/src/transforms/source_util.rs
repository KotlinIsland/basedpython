use std::fmt::Display;

use ruff_python_ast::helpers::consumed_keywords;
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Decorator, Expr, Parameters, Stmt, StmtImportFrom, TypeParam, TypeParams};
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
/// The byte offset a module docstring ends at, including the newline that ends
/// its line — the BOM's length when the module has none.
///
/// A docstring is only a docstring while it is the module's *first* statement,
/// so anything generated ahead of one silently empties the built module's
/// `__doc__`.
pub(crate) fn docstring_end(text: &str) -> usize {
    let bom = usize::from(ruff_source_file::LineRanges::bom_start_offset(text));
    let parsed = ruff_python_parser::parse_unchecked_source(
        &text[bom..],
        ruff_python_ast::PySourceType::Python,
    );
    let Some(Stmt::Expr(first)) = parsed.suite().first() else {
        return bom;
    };
    if !first.value.is_string_literal_expr() {
        return bom;
    }
    line_end(&text[bom..], usize::from(first.range().end())) + bom
}

/// The byte offset generated lines must be spliced at: past the BOM, the module
/// docstring, and any `from __future__ import` — each of which is only valid
/// where it already is.
pub(crate) fn preamble_offset(text: &str) -> usize {
    let mut at = docstring_end(text);
    let parsed = ruff_python_parser::parse_unchecked_source(
        &text[at..],
        ruff_python_ast::PySourceType::Python,
    );
    for stmt in parsed.suite() {
        let Stmt::ImportFrom(import) = stmt else {
            break;
        };
        if import
            .module
            .as_ref()
            .is_none_or(|m| m.as_str() != "__future__")
        {
            break;
        }
        at += line_end(&text[at..], usize::from(import.range().end()));
    }
    at
}

/// the offset just past the end of the line `end` falls on
fn line_end(text: &str, end: usize) -> usize {
    text[end..]
        .find('\n')
        .map_or(text.len(), |newline| end + newline + 1)
}

pub(crate) fn temporary_name(kind: &str, index: impl Display) -> String {
    format!("__by_{kind}_{index}__")
}

/// Where the text between a statement's own words and its value begins.
///
/// A statement's *prefix* is everything it says before that value — `a = ` in
/// `a = 1`, `let a = ` in `let a = 1`, `return ` in `return 1`. `prefix` spans
/// from the statement's start to the value's, and this is the offset within it
/// where the statement stops speaking and mere separation starts: the
/// whitespace after the `=`, an opening parenthesis (an expression's range
/// begins inside any parentheses around it), or a line-continuation backslash.
///
/// A pass that *relocates* a prefix — [`statement_expression`] moves it below
/// the statement it wrapped — re-emits it as a passthrough span so that a pass
/// rewriting the prefix still applies. It cannot re-emit this separator
/// verbatim, because an unmatched `(` would land in the output with its closing
/// `)` left behind in the range being replaced.
///
/// [`statement_expression`]: super::statement_expression
pub(crate) fn value_separator_start(source: &str, prefix: TextRange) -> TextSize {
    let start = usize::from(prefix.start());
    let mut at = usize::from(prefix.end());
    while at > start
        && let byte = source.as_bytes()[at - 1]
        && (byte == b'(' || byte == b'\\' || byte.is_ascii_whitespace())
    {
        at -= 1;
    }
    TextSize::try_from(at).expect("offset within the source fits u32")
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

/// `TypeAlias`, bare or qualified (`typing_extensions.TypeAlias`)
fn is_type_alias_annotation(annotation: &Expr) -> bool {
    match annotation {
        Expr::Name(name) => name.id.as_str() == "TypeAlias",
        Expr::Attribute(attr) => attr.attr.as_str() == "TypeAlias",
        _ => false,
    }
}

impl<F: FnMut(&Expr)> AnnotationWalker<'_, F> {
    /// a pep 695 type parameter's bound and default are type expressions.
    ///
    /// a `constraints` bound is one too, but it is spelled as a parenthesized
    /// tuple, which every caller walks into without rewriting — so it needs no
    /// special case here
    fn walk_type_params(&mut self, type_params: Option<&TypeParams>) {
        let Some(type_params) = type_params else {
            return;
        };
        for type_param in &type_params.type_params {
            let (bound, default) = match type_param {
                TypeParam::TypeVar(tv) => (tv.bound.as_deref(), tv.default.as_deref()),
                TypeParam::TypeVarTuple(tvt) => (None, tvt.default.as_deref()),
                TypeParam::ParamSpec(ps) => (None, ps.default.as_deref()),
            };
            for expr in bound.into_iter().chain(default) {
                (self.on_ann)(expr);
            }
        }
    }

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
            Stmt::AnnAssign(a) => {
                (self.on_ann)(&a.annotation);
                // the value of an `X: TypeAlias = <type>` is itself a type
                // expression — the legacy spelling of `type X = <type>`
                if is_type_alias_annotation(&a.annotation)
                    && let Some(value) = &a.value
                {
                    (self.on_ann)(value);
                }
            }
            Stmt::TypeAlias(a) => {
                self.walk_type_params(a.type_params.as_deref());
                (self.on_ann)(&a.value);
            }
            Stmt::FunctionDef(f) => {
                self.walk_type_params(f.type_params.as_deref());
                self.walk_parameters(&f.parameters);
                if let Some(ret) = &f.returns {
                    (self.on_ann)(ret);
                }
                for s in &f.body {
                    self.visit_stmt(s);
                }
            }
            // a class's bases are deliberately *not* walked: a base is a runtime
            // value position, where the basedpython tuple type is a plain tuple
            // literal — `class C((str, int))` is an `invalid-base` error that
            // raises `TypeError` at runtime, unlike `class C(tuple[str, int])`
            Stmt::ClassDef(c) => {
                self.walk_type_params(c.type_params.as_deref());
                walk_stmt(self, stmt);
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
