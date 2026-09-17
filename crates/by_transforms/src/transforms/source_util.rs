use std::fmt::Display;
use std::fmt::Write as _;

use super::ast_driver::Fragment;
use super::repeated_underscore::WrittenNames;

use ruff_python_ast::helpers::consumed_keywords;
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{
    Decorator, Expr, Parameters, Stmt, StmtFunctionDef, StmtImportFrom, TypeParam, TypeParams,
};
use ruff_python_trivia::SimpleTokenizer;
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

/// Render `value` as a python string literal.
///
/// A conservative escaper that always emits a double-quoted form, so the result
/// re-lexes as one token whatever the text holds.
pub(crate) fn string_repr(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `value` as a string literal node, for a lowering that writes into the syntax
/// tree rather than into the source.
pub(crate) fn string_literal(value: &str) -> ruff_python_ast::ExprStringLiteral {
    ruff_python_ast::ExprStringLiteral {
        node_index: ruff_python_ast::AtomicNodeIndex::NONE,
        range: TextRange::default(),
        value: ruff_python_ast::StringLiteralValue::single(ruff_python_ast::StringLiteral {
            node_index: ruff_python_ast::AtomicNodeIndex::NONE,
            range: TextRange::default(),
            value: value.into(),
            flags: ruff_python_ast::StringLiteralFlags::empty(),
        }),
    }
}

/// the offset just past the end of the line `end` falls on
fn line_end(text: &str, end: usize) -> usize {
    text[end..]
        .find('\n')
        .map_or(text.len(), |newline| end + newline + 1)
}

/// A name for a value the lowering needs to hold on to and the source never named.
///
/// The spelling is chosen to be one nobody writes, but "nobody writes it" is not something
/// the transpiler gets to assume: a module that does write it has the temporary land on top
/// of its own binding, and a read of that binding inside the function the temporary is
/// written in becomes an `UnboundLocalError`. So the name is taken past whatever the module
/// spells, the same way a numbered `_` parameter is.
pub(crate) fn temporary_name(written: WrittenNames<'_>, kind: &str, index: impl Display) -> String {
    written.fresh(&format!("__by_{kind}_{index}__"))
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

/// The source range a value actually occupies, counting any grouping
/// parentheses around it.
///
/// An expression's range starts *inside* its parentheses and ends inside them
/// too, so the value of `let a = (\n    1\n    + 2\n)` is reported as spanning
/// just `1\n    + 2`. A lowering that replaces everything ahead of the value —
/// `let a = ` becomes `a: Final = ` — would then swallow the `(` and leave the
/// `)` behind, three lines down and no longer opened. Walking out over the
/// trivia on both sides recovers the parentheses so the whole group survives
/// the rewrite.
///
/// `limit` is where the walk stops: the end of the last node written *before*
/// the value — an assignment's annotation or target — or the statement's own
/// start when nothing precedes it (a `return`'s operand). It has to be that
/// tight: between a node's end and the value there is only `=`, whitespace,
/// parentheses and comments, so a `#` found there really does open a comment.
/// Starting from the statement instead would scan back across the annotation,
/// where a `#` can sit inside a string.
///
/// Only call this where every parenthesis ahead of the value belongs to the
/// value: an assignment's right-hand side, a `return`'s operand. A call
/// argument's opening parenthesis belongs to the call, not to the argument.
pub(crate) fn parenthesized_value_range(
    source: &str,
    value: TextRange,
    limit: TextSize,
) -> TextRange {
    let bytes = source.as_bytes();
    let lo = usize::from(limit);
    let mut at = usize::from(value.start()).min(source.len());
    let mut start = at;
    let mut depth = 0usize;
    while at > lo {
        match bytes[at - 1] {
            // stepping back over a line break lands at the end of the previous
            // line, where a trailing comment may sit. Nothing between a
            // statement's words and its value can quote a `#`, so the first one
            // on the line starts the comment
            b'\n' => {
                at -= 1;
                let line_start = source[lo..at].rfind('\n').map_or(lo, |i| lo + i + 1);
                if let Some(hash) = source[line_start..at].find('#') {
                    at = line_start + hash;
                }
            }
            b'(' => {
                at -= 1;
                start = at;
                depth += 1;
            }
            byte if byte == b'\\' || byte.is_ascii_whitespace() => at -= 1,
            _ => break,
        }
    }

    // consume exactly as many closing parentheses as were opened, so a
    // parenthesis belonging to an enclosing construct is left where it is
    let mut at = usize::from(value.end()).min(source.len());
    let mut end = at;
    while depth > 0 && at < source.len() {
        match bytes[at] {
            b')' => {
                at += 1;
                end = at;
                depth -= 1;
            }
            b'#' => at += source[at..].find('\n').unwrap_or(source.len() - at),
            byte if byte == b'\\' || byte.is_ascii_whitespace() => at += 1,
            _ => break,
        }
    }

    TextRange::new(
        TextSize::try_from(start).expect("offset within the source fits u32"),
        TextSize::try_from(end).expect("offset within the source fits u32"),
    )
}

/// basedpython: whether `stmt` is one of the declarations the parser
/// synthesized for an `init(…)` shorthand — the `self.<name>: __let__[T] =
/// <name>` that a `let` / `var` parameter stands for.
///
/// These have no source of their own: their ranges point back at the parameter
/// they were built from. A pass that walks type positions would therefore lower
/// the same source twice, once for the parameter's own annotation and once for
/// the synthesized declaration's — landing two copies of the same edit on one
/// range. [`init_method`](super::init_method) writes the real line, so every
/// other pass leaves them alone.
///
/// What identifies them is that the parser gave the statement and its target the
/// *same* range — the parameter's. A statement the source wrote always extends
/// past its target, because the `: T = value` follows it.
pub(crate) fn is_synthesized_init_declaration(stmt: &Stmt) -> bool {
    let (range, target) = match stmt {
        Stmt::AnnAssign(assign) => (assign.range(), assign.target.range()),
        Stmt::Assign(assign) => match assign.targets.first() {
            Some(target) => (assign.range(), target.range()),
            None => return false,
        },
        _ => return false,
    };
    range == target
}

/// Byte offset of the start of the line containing `pos`. Lines begin at
/// either offset 0 or one byte past the previous `\n`
pub(crate) fn line_start(source: &str, pos: TextSize) -> TextSize {
    let offset = usize::from(pos);
    let start = source[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    TextSize::try_from(start).expect("line start fits u32")
}

/// Byte offset just past the newline that ends the line containing `pos` — the
/// end of the source when that line is the last one and carries no newline
pub(crate) fn line_past_end(source: &str, pos: TextSize) -> TextSize {
    let offset = usize::from(pos);
    let end = source[offset..]
        .find('\n')
        .map_or(source.len(), |newline| offset + newline + 1);
    TextSize::try_from(end).expect("line end fits u32")
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

/// basedpython: the offset the binding itself starts at, past any decorators
/// written above it.
///
/// A decorated binding's range covers its decorator lines, the way a decorated
/// `def`'s does. A lowering that replaces everything the statement said ahead of
/// its value has to start here instead of at the statement, because the
/// decorators are not part of what it rewrites — the decorated-binding lowering
/// erases them, and it is the only pass that may
pub(crate) fn binding_start(
    source: &str,
    stmt_range: TextRange,
    decorators: &[Decorator],
) -> TextSize {
    let Some(last) = decorators.last() else {
        return stmt_range.start();
    };
    // a comment may sit between the last decorator and the binding, so trivia is
    // skipped rather than just whitespace
    SimpleTokenizer::new(source, TextRange::new(last.range().end(), stmt_range.end()))
        .skip_trivia()
        .next()
        // nothing after the last decorator would mean a statement with no binding
        // in it, which the parser rejects; falling back past the decorator rather
        // than to the statement keeps even that case from reaching back over them
        .map_or(last.range().end(), |token| token.range.start())
}

/// `value` as a python string literal.
///
/// Rust's own debug spelling is not python: it escapes a control character as
/// `\u{7f}`, which python does not read. Everything printable is written as
/// itself, non-ascii included — the emitted file is the utf-8 python reads by
/// default.
pub(crate) fn python_string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if control.is_control() => {
                let _ = write!(out, "\\x{:02x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
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

/// One statement a [`body_prologue`] insertion writes at the top of a function
/// body.
///
/// `push` appends the statement's fragments; `indent` is the indentation of the
/// body, which a statement that continues onto a second line has to re-establish
/// itself.
pub(crate) trait PrologueStatement {
    fn push(&self, frags: &mut Vec<Fragment>, indent: &str);
}

/// The insertions that write `statements` at the top of `f`'s body — after a
/// docstring, before everything else the source wrote there.
///
/// Usually one insertion. A body the source wrote on the clause's line
/// (`def f(a): """what f does"""`, `def f(a): return 1`) needs a second one:
/// nothing indented can follow a statement there, so the body has to be broken
/// onto a line of its own before anything can be written under it. The break is
/// an insertion of its own, ahead of the body, so that two passes writing into
/// the same body still compose — their statements coalesce at the one offset,
/// exactly as they do for a body that already had a line to itself.
///
/// Whether the body is on the clause's line and where the statements go are two
/// separate questions, because a clause-line body can hold several statements:
/// `def f(a): """doc"""; a.append("x"); return len(a)` breaks at the docstring,
/// which has to stay the body's first statement to stay a docstring, but writes
/// the prologue ahead of the `a.append` that follows it.
///
/// `None` when the body holds nothing the source wrote at all: the `init(…)`
/// shorthand generates its whole body, and an offset taken from a statement the
/// parser synthesized from a parameter points back into the signature.
pub(crate) fn body_prologue(
    source: &str,
    f: &StmtFunctionDef,
    statements: &[impl PrologueStatement],
) -> Option<Vec<(TextSize, Vec<Fragment>)>> {
    let header_end = header_end(f);
    let body_start = first_body_statement(f)
        .and_then(|stmt| body_range(stmt, header_end))?
        .start();
    let mut inserts = Vec::new();
    let body_own_line =
        &source[usize::from(line_start(source, body_start))..usize::from(body_start)];
    let indent = if body_own_line.trim().is_empty() {
        // the insertion lands after the body's own indentation; each statement
        // re-establishes it for the line that follows
        body_own_line.to_owned()
    } else {
        let indent = format!("{}    ", line_indent(source, f.range().start()));
        inserts.push((body_start, vec![Fragment::Lit(format!("\n{indent}"))]));
        indent
    };

    let mut frags: Vec<Fragment> = Vec::new();
    match first_source_statement(f).and_then(|stmt| body_range(stmt, header_end)) {
        Some(range) => {
            let insert_at = range.start();
            // that statement follows a docstring on one line when the break
            // above went somewhere else and left text before it, so a line has
            // to be opened for the prologue as well
            let prefix =
                &source[usize::from(line_start(source, insert_at))..usize::from(insert_at)];
            if insert_at != body_start && !prefix.trim().is_empty() {
                frags.push(Fragment::Lit(format!("\n{indent}")));
            }
            for statement in statements {
                statement.push(&mut frags, &indent);
                frags.push(Fragment::Lit(format!("\n{indent}")));
            }
            inserts.push((insert_at, frags));
        }
        // a docstring is the only thing the source wrote: the insertion follows
        // it, because a docstring is only a docstring while it is the body's
        // first statement
        None => {
            let docstring = source_statements(f)
                .next()
                .filter(|stmt| is_docstring(stmt))
                .and_then(|stmt| body_range(stmt, header_end))?;
            for statement in statements {
                frags.push(Fragment::Lit(format!("\n{indent}")));
                statement.push(&mut frags, &indent);
            }
            inserts.push((docstring.end(), frags));
        }
    }
    Some(inserts)
}

/// The index in `f`'s body that a statement written at the top of it goes at:
/// past a docstring the source wrote, and at 0 when it wrote none.
///
/// This is [`body_prologue`]'s counterpart for a statement printed from its
/// syntax tree rather than spliced into the source. There is no offset to
/// anchor to there, only a position in the body — but the question is the same
/// one, and so is the trap: the parser writes an `init(…)` shorthand's
/// attribute declarations *ahead* of everything the source wrote, so asking
/// whether `body[0]` is a docstring answers no for every `init(…)` that
/// declares an attribute, and the statement lands above the docstring, where
/// the string stops being one and the method loses its `__doc__`.
pub(crate) fn body_prologue_index(f: &StmtFunctionDef) -> usize {
    let header_end = header_end(f);
    f.body
        .iter()
        .position(|stmt| body_range(stmt, header_end).is_some())
        .filter(|&index| is_docstring(&f.body[index]))
        .map_or(0, |index| index + 1)
}

/// The first statement in `f`'s body that came from the source, skipping a
/// docstring and any statement the parser synthesized.
///
/// This is the anchor a body insertion hangs off, and [`init_method`] reads the
/// same one: a body the two disagree about is a body where one of them emits a
/// `_MISSING` sentinel and the other emits no guard for it.
///
/// [`init_method`]: super::init_method
fn first_source_statement(f: &StmtFunctionDef) -> Option<&Stmt> {
    let mut statements = source_statements(f);
    let first = statements.next()?;
    if is_docstring(first) {
        statements.next()
    } else {
        Some(first)
    }
}

/// The statements in `f`'s body the source wrote, in order.
///
/// A body can hold statements the parser synthesized as well — the `init(…)`
/// shorthand's attribute declarations, which it writes *ahead* of everything the
/// source wrote. Asking whether the body's first statement is a docstring would
/// therefore answer no for every `init(…)` that declares an attribute, and put a
/// generated line above the docstring, where it stops being one.
fn source_statements(f: &StmtFunctionDef) -> impl Iterator<Item = &Stmt> {
    let header_end = header_end(f);
    f.body
        .iter()
        .filter(move |s| body_range(s, header_end).is_some())
}

fn is_docstring(stmt: &Stmt) -> bool {
    matches!(stmt, Stmt::Expr(e) if matches!(e.value.as_ref(), Expr::StringLiteral(_)))
}

/// The first statement in `f`'s body that came from the source, a docstring
/// included — that is, whether the source wrote a body at all.
///
/// [`init_method`] hangs the body it generates off this, and it is what
/// [`is_bodyless_init_shorthand`](super::mutable_defaults::is_bodyless_init_shorthand)
/// means by bodyless. A docstring *is* a body: generating another one around it
/// would leave two.
///
/// [`init_method`]: super::init_method
pub(crate) fn first_body_statement(f: &StmtFunctionDef) -> Option<&Stmt> {
    let header_end = header_end(f);
    f.body
        .iter()
        .find(|stmt| body_range(stmt, header_end).is_some())
}

/// The statement's range when it is a position in the *body*. A statement the
/// parser synthesized for an `init(…)` shorthand carries either an empty range
/// or the range of the parameter it was built from, so it points into the
/// header — splicing an insertion there lands it in the middle of the signature.
fn body_range(stmt: &Stmt, header_end: TextSize) -> Option<TextRange> {
    let range = stmt.range();
    (!range.is_empty() && range.start() >= header_end).then_some(range)
}

/// The offset past everything a `def`'s header can span, so a statement before
/// it is one the parser synthesized from a parameter rather than a body.
pub(crate) fn header_end(f: &StmtFunctionDef) -> TextSize {
    f.parameters.range().end().max(
        f.returns
            .as_ref()
            .map_or(TextSize::new(0), |r| r.range().end()),
    )
}
