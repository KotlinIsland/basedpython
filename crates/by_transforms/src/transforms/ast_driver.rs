//! AST → AST rewrite passes for basedpython lowering.
//!
//! Each pass receives the parsed module AST and mutates it. After every
//! pass runs, the driver re-renders each touched top-level statement
//! through [`ruff_python_codegen::Generator`] (basedpython mode) and
//! splices the result back into the source string. The output is then
//! handed to the post-codegen text phases (import-redirect, lazy-import,
//! compat, verify).
//!
//! Capabilities a pass may use:
//!
//! - mutate any expression / statement in place via the
//!   [`Transformer`](ruff_python_ast::visitor::transformer::Transformer)
//!   protocol
//! - declare hoisted statements (new top-level lines that must precede a
//!   particular original statement — e.g. anon-NT class synthesis)
//! - declare required imports (full `import …` / `from … import …` lines
//!   that the driver prepends to the source)
//! - declare sub-statement text edits, for rewrites that would otherwise leak
//!   when an outer transform copies operand source verbatim (variance keyword
//!   blanking)
//!
//! AST passes always engage — there is no gate. The text-edit pipeline
//! is intentionally not invoked for any construct an AST pass handles.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeSet;

use ruff_python_ast::visitor::transformer::Transformer;
use ruff_python_ast::{Expr, ModModule, PySourceType, Stmt};
use ruff_python_codegen::{Generator, Indentation, Mode};
use ruff_python_parser::parse_unchecked_source;
use ruff_source_file::LineEnding;
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::source_util::preamble_offset;
use super::{
    annotation, anon_named_tuple, auto_quote, build_stamps, callable, character_type, checked_cast,
    class_pattern_star, coalesce, compat, conformance, context_params, conversion, debug_field,
    decl_site_variance, decorated_binding, decorator_keyword, dedent_string, destructure,
    django_lookup, dynamic_keyword, empty_declarations, export_import, extension, flexible_keyword,
    float_const, force_unwrap, frameworks, generic_call, generics, grapheme_string, identity_swap,
    if_let, implicit_receiver, implicit_typing, inferred_annotation, init_method, just_float,
    kw_subscript, literal_string, literal_types, local_once, main_function, match_type, modifiers,
    module_api, mutable_defaults, none_chain, optional_type, overload, parametric_is,
    postfix_await, propagate, properties, protocol_type, raises_clause, reified_class,
    reified_generic, repeated_underscore, return_value_use, runtime_union, sentinel, some_ctor,
    soundness, statement_expression, static_resource, string_tag, super_keyword, symbolic_type_op,
    template_expression, template_type, top_star, trailing_lambda, tuple_index, type_fn, type_is,
    type_reification, typed_dict_literal, typed_lambda, typeof_keyword, unique_loop_bindings,
    unpack, use_site_variance, visibility_rename,
};
use crate::Config;
use crate::source_map::Replacement;
use crate::type_info::TypeInfo;

/// Holds the db backing the type-aware passes. `Local` owns a single-file
/// in-memory db; `Project` borrows the caller's project db (cross-module
/// imports resolve). Either way the parse + [`SemanticModel`] the passes use
/// come from this one db, preserving `inferred_type` node-identity lookups.
enum SemDb<'p> {
    Project(&'p dyn ty_python_semantic::Db, ruff_db::files::File),
    Local(ty_project::TestDb, ruff_db::files::File),
}

/// One fragment of a [`PassContext::template_edits`] replacement: literal text,
/// or a passthrough span of original source. Passthrough spans are materialized
/// with any sibling edits inside them applied, so a wide rewrite (e.g.
/// `a ?? b` → `a if a is not None else b`) composes with lowerings inside its
/// operands instead of clobbering them via first-wins overlap dedup
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Fragment {
    Lit(String),
    Src(TextRange),
}

/// Mutable state shared across every pass during a single transpile.
#[derive(Default)]
pub(crate) struct PassContext {
    /// Top-level statements that any pass inserted. Each entry is
    /// `(insert_before_idx, stmt)` where `insert_before_idx` is the
    /// 0-based index in the **original** module body before which the
    /// new statement should appear. Multiple inserts at the same idx
    /// preserve declared order.
    pub(crate) hoisted: Vec<(usize, Stmt)>,
    /// Full source lines to prepend to the file (e.g. `from typing import cast`).
    /// Deduped before emission.
    pub(crate) required_imports: Vec<String>,
    /// Runtime helpers the emitted code calls, by the name it calls them under.
    /// The driver turns these into an import of the module a build writes them
    /// to, or — when there is no such module — into the definitions themselves.
    /// See [`crate::runtime`]
    pub(crate) runtime: BTreeSet<crate::runtime::Helper>,
    /// Indices into the *original* module body of statements any pass
    /// mutated (so the driver knows to re-render them). Indices may
    /// repeat — the driver dedupes.
    pub(crate) changed: Vec<usize>,
    /// Sub-statement text edits: `(range_in_source, replacement)`. Used
    /// by passes that rewrite a single sub-expression (e.g. an annotation
    /// inside a `final def` signature) without disturbing the rest of the
    /// statement. Avoids whole-statement codegen for cases where the
    /// surrounding context contains basedpython markers a sibling pass
    /// hasn't lowered yet
    pub(crate) text_edits: Vec<(TextRange, String)>,
    /// Structured sub-statement edits whose replacement is a [`Fragment`] list.
    /// Unlike `text_edits` (whose plain string wins over anything nested inside
    /// it — which is a transpile error unless the pass writes that lowering itself,
    /// see [`TypeAwarePass::subsumes`]), the `Src` passthrough spans of a template are
    /// materialized with the sibling edits they contain applied — use this for any
    /// rewrite that re-emits operand source
    pub(crate) template_edits: Vec<(TextRange, Vec<Fragment>)>,
    /// Templates inserted at a *statement* boundary — a guard a pass injects
    /// ahead of the statement starting at that offset. Identical to a
    /// zero-width [`template_edits`](Self::template_edits) entry except that a
    /// rewrite of the statement's own first expression cannot absorb it: a
    /// statement materialized inside an expression is a syntax error, not a
    /// composition
    pub(crate) statement_inserts: Vec<(TextSize, Vec<Fragment>)>,
    /// Sub-statement edits standing in for a construct the same pass re-emits
    /// somewhere else: the `_MISSING` a mutable default leaves in the
    /// signature, whose written value the body guard evaluates instead.
    ///
    /// Identical to a [`template_edits`](Self::template_edits) entry except
    /// that it leads every other edit at its span. The others are rewrites of
    /// the construct, and the construct is no longer here — they materialize
    /// where the pass re-emits it. Without this they would be ordered against
    /// the substitution by shape alone, which cannot tell two substitutions of
    /// one span apart
    pub(crate) relocating_edits: Vec<(TextRange, Vec<Fragment>)>,
    /// Soundness checks made in top-level statements an AST pass rewrote, by the
    /// statement's index in the original module body. The nodes a pass changed are
    /// printed from the syntax tree, where a text edit made at a node the pass
    /// replaced does not land, so these are written into that tree before it is
    /// printed
    pub(crate) rerendered_checks: Vec<(usize, Vec<super::soundness::RerenderedCheck>)>,
    /// Hard transpile errors a pass surfaced — abort the pipeline rather
    /// than emit partial / invalid output. Each entry is a human-readable
    /// message suitable for showing the user
    pub(crate) errors: Vec<String>,
    /// Lines to append AFTER the spliced body (e.g. modifiers' auto-
    /// generated `__all__ = [...]`). Driver emits each as its own line
    pub(crate) epilogue: Vec<String>,
    /// Import lines a *synthesized* type expression needs that the source never
    /// wrote (`import decimal` for an inferred `decimal.Decimal` annotation).
    /// The driver emits these under `if TYPE_CHECKING:` — the output always
    /// carries `from __future__ import annotations`, so the name is only ever
    /// read by a checker, and a runtime import here would add an import edge the
    /// source does not have
    pub(crate) type_only_imports: BTreeSet<String>,
    /// Source ranges of operations that `symbolic_type_op` resolved up front
    /// (e.g. `1 + 1` → `Literal[2]`). Type-aware passes skip these via
    /// [`walk_type_positions_skipping`](super::type_expr_walker::walk_type_positions_skipping)
    /// so they don't re-process an operation that no longer appears in the output
    pub(crate) claimed_type_op_ranges: Vec<TextRange>,
    /// The `typeof` nodes nested under a structural type form, which the
    /// type-expression lowerer rewrites as part of that form. Every other `typeof`
    /// is lowered in the syntax tree, and only there
    pub(crate) structural_typeof_ranges: Vec<TextRange>,
    /// The same operations as `(range, rendered)` pairs. A pass that replaces a
    /// whole statement subsumes any fold inside it — skipping is not enough, the
    /// rendered text has to be spliced into the replacement or the operation is
    /// re-emitted from source and reaches the runtime
    pub(crate) symbolic_substitutions: Vec<(TextRange, String)>,
}

/// a lowering whose edits another pass may leave out of its own, because that pass writes
/// the construct the edit lowers itself — see [`TypeAwarePass::subsumes`]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lowering {
    AnonNamedTuple,
    Callable,
    ContextParams,
    Conversion,
    DynamicKeyword,
    FloatConst,
    GenericPolyfill,
    InferredAnnotation,
    JustFloat,
    /// the whitespace the keyword blanking collapses. only spacing is at stake, so any
    /// edit it lands inside may leave it out
    KeywordPadding,
    LiteralType,
    LocalOnce,
    MatchType,
    Modifiers,
    NoneChain,
    NoneCoalesce,
    OptionalType,
    ParametricIs,
    ProtocolType,
    RepeatedUnderscore,
    RuntimeUnion,
    StatementExpression,
    SymbolicTypeOp,
    TupleLiteralType,
    Typeof,
    Unpack,
    VarianceStrip,
    VisibilityRename,
}

/// A single AST-level rewrite pass.
pub(crate) trait AstPass {
    /// Run the pass against the entire parsed module. The pass is free
    /// to mutate any statement in place, declare hoisted statements,
    /// and request runtime imports via [`PassContext`].
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext);

    /// See [`TypeAwarePass::runtime_only`].
    fn runtime_only(&self) -> bool {
        false
    }

    /// See [`TypeAwarePass::lowering`].
    fn lowering(&self) -> Option<Lowering> {
        None
    }

    /// See [`TypeAwarePass::subsumes`].
    fn subsumes(&self) -> &'static [Lowering] {
        &[]
    }

    /// what a transform conflict calls the pass
    fn name(&self) -> &'static str {
        short_type_name(std::any::type_name::<Self>())
    }
}

/// Type-aware pass that reads semantic info from the salsa-owned parsed
/// module + [`SemanticModel`]. Operates strictly via [`PassContext`]
/// `text_edits` / `required_imports`; the input AST is shared & immutable
/// because `inferred_type` queries bind to its exact node identities
pub(crate) trait TypeAwarePass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext);

    /// whether everything this pass emits exists for what the code does when
    /// it runs — a check, a registration, an entry point — rather than to spell
    /// something a checker reads. a stub is read and never run, so the driver
    /// leaves such a pass out of a stub's transpile
    ///
    /// a pass that lowers syntax must never say so: left out, its construct
    /// would reach the output as something python cannot parse
    fn runtime_only(&self) -> bool {
        false
    }

    /// the lowering this pass's edits make, when another pass may write the constructs
    /// they lower itself — see [`subsumes`](Self::subsumes)
    fn lowering(&self) -> Option<Lowering> {
        None
    }

    /// the lowerings whose constructs this pass writes itself, wherever one of its edits
    /// covers them: an edit such a lowering makes inside one of this pass's edits is left
    /// out of the output, and that is not a loss
    ///
    /// every other edit inside one of this pass's edits has to land in source the edit
    /// passes through, or the transpile is refused. so a lowering is listed here only
    /// where this pass writes what that lowering would have written, and a lowering added
    /// later that reaches inside one of its edits is refused until it is
    fn subsumes(&self) -> &'static [Lowering] {
        &[]
    }

    /// what a transform conflict calls the pass
    fn name(&self) -> &'static str {
        short_type_name(std::any::type_name::<Self>())
    }
}

/// `path::to::Type<Args>` as `Type`
fn short_type_name(name: &'static str) -> &'static str {
    let bare = name.split('<').next().unwrap_or(name);
    bare.rsplit("::").next().unwrap_or(bare)
}

/// Adapter: lift a [`Transformer`] (visitor that mutates AST in place)
/// into an [`AstPass`] that auto-tracks which top-level statements
/// changed. The transformer must record its mutation status into the
/// supplied `Cell<bool>`; the adapter resets the cell per statement.
pub(crate) struct VisitorPass<'a, T: Transformer> {
    inner: &'a T,
    changed_cell: &'a std::cell::Cell<bool>,
    imports: Vec<String>,
    hoist: RefCell<Vec<(usize, Stmt)>>,
    /// Sub-statement text edits the pass wants the driver to apply. Pass
    /// computes the new sub-AST, renders it via [`render_expr`], and pushes
    /// `(original_range, replacement)` here
    text_edits: RefCell<Vec<(TextRange, String)>>,
    lowering: Option<Lowering>,
}

impl<T: Transformer> AstPass for VisitorPass<'_, T> {
    fn lowering(&self) -> Option<Lowering> {
        self.lowering
    }

    fn name(&self) -> &'static str {
        short_type_name(std::any::type_name::<T>())
    }

    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        for (idx, stmt) in module.body.iter_mut().enumerate() {
            self.changed_cell.set(false);
            self.inner.visit_stmt(stmt);
            if self.changed_cell.get() {
                ctx.changed.push(idx);
            }
        }
        ctx.required_imports.extend(self.imports.iter().cloned());
        ctx.hoisted.extend(self.hoist.borrow_mut().drain(..));
        ctx.text_edits
            .extend(self.text_edits.borrow_mut().drain(..));
    }
}

/// Render a statement back to python source using ruff's [`Generator`].
/// Basedpython mode handles surviving basedpython-only AST nodes.
pub(crate) fn render_stmt(stmt: &Stmt) -> String {
    let indent = Indentation::default();
    Generator::new(&indent, LineEnding::Lf)
        .with_mode(Mode::BasedPython)
        .stmt(stmt)
}

/// Render an expression back to python source using ruff's [`Generator`].
/// Used by passes that emit sub-statement text edits.
pub(crate) fn render_expr(expr: &Expr) -> String {
    let indent = Indentation::default();
    Generator::new(&indent, LineEnding::Lf)
        .with_mode(Mode::BasedPython)
        .expr(expr)
}

/// A sub-statement edit's replacement: plain text, or a template whose `Src`
/// passthrough spans compose with nested edits.
enum SubPatch {
    Text(String),
    Template(Vec<Fragment>),
    /// a template anchored at a statement boundary — see
    /// [`PassContext::statement_inserts`]. Materializes exactly like
    /// [`SubPatch::Template`]; the difference is only in what may claim it
    Statement(Vec<Fragment>),
    /// a template standing in for a construct the same pass re-emits somewhere
    /// else — see [`PassContext::relocating_edits`]. Materializes exactly like
    /// [`SubPatch::Template`]; the difference is only that it leads every other
    /// edit at its span
    Relocating(Vec<Fragment>),
    /// a node an AST pass rewrote, re-emitted at its range — see
    /// [`super::rerender`]. Materializes exactly like [`SubPatch::Template`], and
    /// leads the other rewrites at its span, as printing the whole statement
    /// used to. an insertion at its boundary is not absorbed: the text it prints
    /// there is the node's own, which no insertion targets
    Rerendered(Vec<Fragment>),
}

/// whether a template spanning `start..end` only wraps it: its one passthrough
/// is the whole span, and everything else it emits is text around it
fn is_wrapper(frags: &[Fragment], start: usize, end: usize) -> bool {
    let mut passthroughs = frags.iter().filter_map(|frag| match frag {
        Fragment::Src(range) => Some(range),
        Fragment::Lit(_) => None,
    });
    passthroughs
        .next()
        .is_some_and(|range| usize::from(range.start()) == start && usize::from(range.end()) == end)
        && passthroughs.next().is_none()
}

/// The sub-edits a template materializes, in position order: those nested in
/// its own range, plus those its `Src` passthrough spans contain.
///
/// The two normally coincide, because a template re-emits the span it replaces.
/// A *relocating* template passes source through from somewhere else — the
/// default re-evaluation guard re-emits a parameter default at the body start —
/// and the lowerings inside that span have to materialize where the source
/// lands, not where it was written.
///
/// A [`SubPatch::Relocating`] edit is the one thing that does *not* travel with
/// that source. It is the stand-in a pass leaves at the span it moved a
/// construct away from — `_MISSING` where the default was written — and a
/// passthrough of exactly that span, from outside the template's own range, is
/// the place the construct moved *to*. Materializing the stand-in there too
/// would put `_MISSING` at both ends and the construct at neither, which is how
/// a `decorator def` whose header template re-emits the whole signature came to
/// emit `if tags is _MISSING: tags = _MISSING`.
fn template_claimees(
    frags: &[Fragment],
    all: &[(usize, usize, SubPatch)],
    claimed: &[bool],
    self_idx: usize,
    own: Option<(usize, usize)>,
) -> Vec<usize> {
    (0..all.len())
        .filter(|&m| m != self_idx && claimed[m])
        .filter(|&m| {
            let (s, e) = (all[m].0, all[m].1);
            let in_own = own.is_some_and(|(start, end)| s >= start && e <= end && s != end);
            // a boundary insertion at a passthrough's end is left to
            // `apply_within`'s `include_end`, which knows whether an adjacent
            // span will re-emit it
            let in_src = frags.iter().any(|frag| match frag {
                Fragment::Lit(_) => false,
                Fragment::Src(span) => {
                    let (span_start, span_end) =
                        (usize::from(span.start()), usize::from(span.end()));
                    s >= span_start
                        && e <= span_end
                        && !(matches!(all[m].2, SubPatch::Relocating(_))
                            && (s, e) == (span_start, span_end))
                }
            });
            in_own || in_src
        })
        .collect()
}

/// Whether a template over `range` puts text there other than the text already
/// written there.
///
/// Read fragment by fragment against the covered source, so neither side is
/// built up: a literal has to match the source ahead of it, and a passthrough
/// has to be the very span it stands at.
///
/// A `Src` passthrough carries any sibling edit that falls inside it, which this
/// reading leaves out — but such a sibling is an entry of the same edit list, and
/// the caller asks the list as a whole. So a template that only re-emits what it
/// covers answers `false` here while a real rewrite nested inside it still
/// answers `true` under its own entry.
fn rewrites_covered_source(range: TextRange, frags: &[Fragment], source: &str) -> bool {
    let Some(covered) = source.get(usize::from(range.start())..usize::from(range.end())) else {
        return true;
    };
    let mut rest = covered;
    for frag in frags {
        let piece = match frag {
            Fragment::Lit(text) => text.as_str(),
            Fragment::Src(span) => {
                match source.get(usize::from(span.start())..usize::from(span.end())) {
                    Some(text) => text,
                    None => return true,
                }
            }
        };
        match rest.strip_prefix(piece) {
            Some(tail) => rest = tail,
            None => return true,
        }
    }
    !rest.is_empty()
}

/// Materialize a template's fragments into `out`. `Src` passthrough spans are
/// emitted from original source with the contained sub-edits (indices into
/// `all`) applied.
///
/// `anchor` is the start of the edit the template belongs to. Its literal text
/// is charged to that offset in the line table: a hoisted `def` header, an
/// injected keyword argument, a `nonlocal` line — none of them has a line of
/// its own, and the construct the edit rewrites is the one they stand for.
fn materialize_fragments(
    out: &mut Replacement,
    frags: &[Fragment],
    source: &str,
    all: &[(usize, usize, SubPatch)],
    contained: &[usize],
    anchor: usize,
    emitted: &mut [bool],
) {
    for (i, frag) in frags.iter().enumerate() {
        match frag {
            Fragment::Lit(s) => out.push_generated(s, anchor),
            Fragment::Src(span) => {
                // a zero-width insertion at this span's end is normally deferred
                // to the *next* `Src` span (which re-emits it at its start), so
                // two adjacent passthroughs don't both emit it. but when no
                // adjacent `Src` follows (the next fragment is literal text, or
                // this is the last fragment) there is nothing to defer it to, so
                // this span must emit it — otherwise a wrap whose closing token
                // sits at the span boundary (a reified `[1]` → `list[int]([1])`)
                // loses that token
                let include_end = !matches!(
                    frags.get(i + 1),
                    Some(Fragment::Src(next)) if next.start() == span.end()
                );
                apply_within(
                    out,
                    source,
                    usize::from(span.start()),
                    usize::from(span.end()),
                    all,
                    contained,
                    include_end,
                    emitted,
                );
            }
        }
    }
}

/// Emit `source[s0..e0]` with the edits from `contained` (indices into `all`,
/// in position order) that fall inside the span applied, first-wins on
/// overlap. Nested templates recurse; a same-start insertion at depth ≥ 2 is
/// emitted ahead of its nested template rather than absorbed into it (only the
/// top-level claim pass implements absorption). `statement_expression` nests
/// templates — one statement expression inside another's suite, and any pass
/// whose template lands inside the suite it passes through. `include_end` controls
/// whether a zero-width insertion exactly at `e0` is emitted here (see
/// [`materialize_fragments`]). every edit written out is marked in `emitted`
#[expect(clippy::too_many_arguments)]
fn apply_within(
    out: &mut Replacement,
    source: &str,
    s0: usize,
    e0: usize,
    all: &[(usize, usize, SubPatch)],
    contained: &[usize],
    include_end: bool,
    emitted: &mut [bool],
) {
    let mut cursor = s0;
    let mut k = 0;
    while k < contained.len() {
        let idx = contained[k];
        let (s, e) = (all[idx].0, all[idx].1);
        // outside this span, a deferred boundary insertion at its end, or
        // overlapping an already-applied edit — skip
        if s < cursor || s < s0 || e > e0 || (!include_end && s == e && s == e0) {
            k += 1;
            continue;
        }
        out.push_source(source, cursor, s);
        emitted[idx] = true;
        match &all[idx].2 {
            SubPatch::Text(t) => out.push_generated(t, s),
            SubPatch::Template(frags)
            | SubPatch::Statement(frags)
            | SubPatch::Relocating(frags)
            | SubPatch::Rerendered(frags) => {
                let inner: Vec<usize> = contained[k + 1..]
                    .iter()
                    .copied()
                    .filter(|&m| all[m].0 >= s && all[m].1 <= e && all[m].0 != e)
                    .collect();
                materialize_fragments(out, frags, source, all, &inner, s, emitted);
            }
        }
        cursor = cursor.max(e);
        k += 1;
    }
    out.push_source(source, cursor, e0);
}

/// the sub-statement edits spliced into the source, first-wins on overlap, and an error
/// for each edit the splice loses. `origins` names, by index into `authors`, who wrote
/// each edit
///
/// an edit is left out when a wider edit it sits inside prints text of its own over it: a
/// node an AST pass rewrote that no longer passes the edit's source through, a template
/// that writes the construct itself, or a plain-text replacement. whatever the edit made of
/// the construct is then gone — whether or not the printed text happens to spell the
/// construct again — so that is refused rather than emitted, unless the wider edit
/// accounts for it (see [`lost_edits`])
fn splice(
    source: &str,
    sub_edits: Vec<(usize, usize, SubPatch)>,
    origins: Vec<Vec<usize>>,
    authors: &[Author],
) -> (Vec<(usize, usize, Replacement)>, Vec<String>) {
    let mut edits: Vec<(usize, usize, Replacement)> = Vec::new();
    let mut emitted = vec![false; sub_edits.len()];
    let mut tagged: Vec<_> = sub_edits.into_iter().zip(origins).collect();
    // start asc. tie-break by edit shape:
    //   1. zero-width insertions first — they don't consume bytes, so any
    //      following deletion/replacement at the same start can still apply.
    //      a statement-anchored insertion leads them: it emits whole statements
    //      that must precede everything the statement itself lowers to
    //   2. then wider replacements before narrower ones — so a wider edit
    //      wins over (or, for templates, absorbs) a narrow one nested inside
    //      it
    //   3. at one identical span, a *relocating* edit leads: it says the
    //      construct has moved, and the pass that moved it re-emits the span
    //      itself, so every other edit there materializes at the new home
    //   4. then a *wrapper* — a template whose one passthrough is its whole
    //      span, adding text around the construct without removing any of it.
    //      it claims the other edits at that span and materializes them inside
    //      its passthrough, so whatever the construct becomes ends up inside the
    //      wrapping (a quoted forward reference around an arrow type the
    //      callable lowering replaced as text)
    //   5. then a node an AST pass rewrote, which the syntax tree says the
    //      construct became — see [`super::rerender`]
    //   6. then a *substitution* — plain text, or a template with no `Src`
    //      passthrough — ahead of a *rewrite*, a template that re-emits part of
    //      the span. a substitution says the construct does not appear here at
    //      all, which a rewrite of it cannot outrank
    tagged.sort_by(|(a, _), (b, _)| {
        let priority = |e: &(usize, usize, SubPatch)| {
            let rewrites = i64::from(match &e.2 {
                SubPatch::Text(_) => false,
                SubPatch::Template(frags)
                | SubPatch::Statement(frags)
                | SubPatch::Relocating(frags)
                | SubPatch::Rerendered(frags) => {
                    frags.iter().any(|frag| matches!(frag, Fragment::Src(_)))
                }
            });
            let statement = i64::from(!matches!(e.2, SubPatch::Statement(_)));
            let relocating = i64::from(!matches!(e.2, SubPatch::Relocating(_)));
            let wraps = i64::from(
                !matches!(&e.2, SubPatch::Template(frags) if is_wrapper(frags, e.0, e.1)),
            );
            let rerendered = i64::from(!matches!(e.2, SubPatch::Rerendered(_)));
            // (start, is_replacement_not_insertion, statement-insert-first,
            //  neg_end-for-wider-first, relocating-first, wrapper-first,
            //  rerendered-first, substitution-before-rewrite)
            if e.1 == e.0 {
                (
                    e.0, 0i64, statement, 0i64, relocating, wraps, rerendered, rewrites,
                ) // insertion
            } else {
                #[allow(clippy::cast_possible_wrap)]
                let neg_end = -(e.1 as i64);
                (
                    e.0, 1i64, statement, neg_end, relocating, wraps, rerendered, rewrites,
                )
            }
        };
        priority(a).cmp(&priority(b))
    });
    let (sub_edits, origins): (Vec<_>, Vec<_>) = tagged.into_iter().unzip();
    // claim pre-pass: each replacement, outermost-first (the sort guarantees an
    // enclosing edit precedes anything inside it), claims the edits nested in
    // its span. a template *materializes* its claimees inside its `Src` spans;
    // a plain-text replacement drops them (first-wins). same-start zero-width
    // insertions are absorbed by a template (they target the construct's first
    // token, e.g. `_force_unwrap(` ahead of a coalesce operand) but stay
    // independent ahead of a plain-text replacement, preserving the documented
    // insertion + deletion compose behaviour. the one exception is a
    // statement-anchored insertion sharing a boundary: it emits statements, so
    // absorbing it into an expression rewrite of the statement it precedes
    // would splice a suite into the middle of an expression
    let mut claimed = vec![false; sub_edits.len()];
    for i in 0..sub_edits.len() {
        let (s_i, e_i) = (sub_edits[i].0, sub_edits[i].1);
        if e_i == s_i || claimed[i] {
            continue;
        }
        // a rerendered node claims what is inside it as a template does, but not an
        // insertion at its boundary, which targets the source around it
        let is_template = matches!(
            sub_edits[i].2,
            SubPatch::Template(_) | SubPatch::Statement(_) | SubPatch::Relocating(_)
        );
        for (m, edit) in sub_edits.iter().enumerate() {
            if m == i || claimed[m] {
                continue;
            }
            let (s_m, e_m) = (edit.0, edit.1);
            let inside = s_m >= s_i && e_m <= e_i && s_m != e_i;
            let boundary_insertion = s_m == e_m && (s_m == s_i || s_m == e_i);
            let anchored = matches!(edit.2, SubPatch::Statement(_));
            if inside && (is_template || !boundary_insertion) && !(boundary_insertion && anchored) {
                claimed[m] = true;
            }
        }
    }

    let mut cursor = 0usize;
    let mut i = 0;
    while i < sub_edits.len() {
        if claimed[i] {
            i += 1;
            continue;
        }
        let (start, end) = (sub_edits[i].0, sub_edits[i].1);
        if start < cursor {
            i += 1;
            continue;
        }
        // coalesce all unclaimed zero-width insertions sharing this start into
        // a single combined insertion (text concatenated in push order). this
        // sidesteps the replace_range-at-same-position ordering issue: each
        // pass pushes its slice in left-to-right intent order, and we
        // splice them as one contiguous string
        if end == start {
            let mut combined = Replacement::default();
            let mut j = i;
            while j < sub_edits.len() && sub_edits[j].0 == start && sub_edits[j].1 == start {
                if !claimed[j] {
                    emitted[j] = true;
                    match &sub_edits[j].2 {
                        SubPatch::Text(t) => combined.push_generated(t, start),
                        SubPatch::Template(frags)
                        | SubPatch::Statement(frags)
                        | SubPatch::Relocating(frags)
                        | SubPatch::Rerendered(frags) => {
                            let contained = template_claimees(frags, &sub_edits, &claimed, j, None);
                            materialize_fragments(
                                &mut combined,
                                frags,
                                source,
                                &sub_edits,
                                &contained,
                                start,
                                &mut emitted,
                            );
                        }
                    }
                }
                j += 1;
            }
            edits.push((start, start, combined));
            i = j;
            continue;
        }
        emitted[i] = true;
        let repl = match &sub_edits[i].2 {
            // a plain-text replacement wins over anything inside it
            SubPatch::Text(t) => Replacement::generated(t, start),
            SubPatch::Template(frags)
            | SubPatch::Statement(frags)
            | SubPatch::Relocating(frags)
            | SubPatch::Rerendered(frags) => {
                // the claimees nested in this span materialize inside the
                // template's `Src` passthrough fragments
                let contained =
                    template_claimees(frags, &sub_edits, &claimed, i, Some((start, end)));
                let mut out = Replacement::default();
                materialize_fragments(
                    &mut out,
                    frags,
                    source,
                    &sub_edits,
                    &contained,
                    start,
                    &mut emitted,
                );
                out
            }
        };
        edits.push((start, end, repl));
        cursor = end;
        i += 1;
    }

    let lost = lost_edits(source, &sub_edits, &origins, authors, &emitted);
    (edits, lost)
}

/// the errors for the edits in `sub_edits`, in the order it is sorted in, that the splice
/// left out of its output — those not marked in `emitted` — and that the edit they were
/// left out for does not account for
///
/// what an edit was left out for is the innermost edit around it, or at exactly its range
/// when that edit sorts ahead of it — the nearest, of several there. that edit accounts
/// for it when:
///
/// - it deletes what it covers, so nothing inside is printed at all
/// - it is at the same span and writes the same thing, as two lowerings of one construct
///   can
/// - one pass wrote both, and so wrote them to agree
/// - its pass writes the constructs of every pass the lost edit came from itself — see
///   [`TypeAwarePass::subsumes`]
///
/// a node an AST pass rewrote accounts for nothing: it prints what the syntax tree says,
/// which no edit of the source reaches
fn lost_edits(
    source: &str,
    sub_edits: &[(usize, usize, SubPatch)],
    origins: &[Vec<usize>],
    authors: &[Author],
    emitted: &[bool],
) -> Vec<String> {
    let mut errors = Vec::new();
    for (index, (start, end, _)) in sub_edits.iter().enumerate() {
        if emitted[index] {
            continue;
        }
        let (start, end) = (*start, *end);
        let around = sub_edits
            .iter()
            .enumerate()
            .filter(|&(other, (other_start, other_end, _))| {
                let (other_start, other_end) = (*other_start, *other_end);
                other != index
                    && if (other_start, other_end) == (start, end) {
                        other < index
                    } else {
                        other_start <= start && end <= other_end && other_start < other_end
                    }
            })
            // of two around it at one span, the later one sorts nearer to it
            .min_by_key(|&(other, (other_start, other_end, _))| {
                (other_end - other_start, std::cmp::Reverse(other))
            })
            .map(|(other, (_, _, patch))| (other, patch));
        let lost: Vec<&Author> = origins[index]
            .iter()
            .filter_map(|&author| authors.get(author))
            .collect();
        let only_spacing = !lost.is_empty()
            && lost
                .iter()
                .all(|author| author.lowering == Some(Lowering::KeywordPadding));
        if only_spacing {
            continue;
        }
        if let Some((other, patch)) = around {
            let deletes = matches!(patch, SubPatch::Text(text) if text.is_empty());
            let same = (sub_edits[other].0, sub_edits[other].1) == (start, end)
                && writes_the_same(patch, &sub_edits[index].2);
            let winner = match origins[other].as_slice() {
                [author] if !matches!(patch, SubPatch::Rerendered(_)) => Some(*author),
                _ => None,
            };
            let accounted = winner.is_some_and(|winner| {
                !origins[index].is_empty()
                    && origins[index].iter().all(|&author| {
                        author == winner
                            || authors[author].lowering.is_some_and(|lowering| {
                                authors[winner].subsumes.contains(&lowering)
                            })
                    })
            });
            if deletes || same || accounted {
                continue;
            }
        }
        let what = if start == end {
            format!("text inserted at byte {start}")
        } else {
            let preview: String = source[start..end].chars().take(40).collect();
            format!("an edit of `{preview}`")
        };
        let by = match lost.as_slice() {
            [] => String::new(),
            names => format!(
                " by {}",
                names
                    .iter()
                    .map(|author| format!("`{}`", author.name))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
        };
        let lost_to = match around {
            Some((_, SubPatch::Rerendered(_))) => {
                "lands inside a node an AST pass rewrote and prints again".to_owned()
            }
            Some((other, _)) => {
                let name = match origins[other].as_slice() {
                    [author] => authors.get(*author).map_or("another pass", |a| a.name),
                    _ => "another pass",
                };
                format!("lands inside a construct `{name}` writes itself")
            }
            None => "overlaps a construct another edit rewrote".to_owned(),
        };
        errors.push(format!(
            "transform conflict: {what}{by} {lost_to}, so the lowering would be lost"
        ));
    }
    errors
}

/// whether `a` and `b`, two edits of one span, put the same thing there
fn writes_the_same(a: &SubPatch, b: &SubPatch) -> bool {
    match (a, b) {
        (SubPatch::Text(a), SubPatch::Text(b)) => a == b,
        (
            SubPatch::Template(a) | SubPatch::Statement(a) | SubPatch::Relocating(a),
            SubPatch::Template(b) | SubPatch::Statement(b) | SubPatch::Relocating(b),
        ) => a == b,
        _ => false,
    }
}

/// a pass, or a part of the driver, that writes edits into a [`PassContext`]
pub(crate) struct Author {
    name: &'static str,
    lowering: Option<Lowering>,
    subsumes: &'static [Lowering],
}

impl Author {
    /// the driver writing edits of its own
    fn driver(name: &'static str, lowering: Option<Lowering>) -> Self {
        Self {
            name,
            lowering,
            subsumes: &[],
        }
    }
}

/// the edit lists of a [`PassContext`], in the order the driver chains them
#[derive(Clone, Copy)]
enum EditList {
    Text,
    Template,
    Statement,
    Relocating,
}

/// who wrote each edit in a [`PassContext`] and rewrote each statement, read off how long
/// each list was as each author finished — the lists only ever grow while passes run
#[derive(Default)]
struct Authorship {
    authors: Vec<Author>,
    /// the length of the four edit lists, then of `changed`, as each author finished
    marks: Vec<[usize; 5]>,
}

#[cfg(test)]
thread_local! {
    /// the lowerings each pass declares it writes itself, by the pass's name, recorded as
    /// the passes run: the test holding every declaration to a case showing it reads them
    static DECLARED_SUBSUMPTIONS: RefCell<Vec<(&'static str, &'static [Lowering])>> =
        const { RefCell::new(Vec::new()) };
}

impl Authorship {
    fn finished(&mut self, author: Author, ctx: &PassContext) {
        #[cfg(test)]
        DECLARED_SUBSUMPTIONS
            .with(|declared| declared.borrow_mut().push((author.name, author.subsumes)));
        self.authors.push(author);
        self.marks.push([
            ctx.text_edits.len(),
            ctx.template_edits.len(),
            ctx.statement_inserts.len(),
            ctx.relocating_edits.len(),
            ctx.changed.len(),
        ]);
    }

    /// the author of the edit at `index` in `list`
    fn author_of(&self, list: EditList, index: usize) -> Option<usize> {
        self.marks
            .iter()
            .position(|mark| index < mark[list as usize])
    }

    /// the authors that rewrote each statement, by its index in `changed`, the context's
    /// list of rewritten statements before it is sorted
    fn rewriters(&self, changed: &[usize]) -> std::collections::HashMap<usize, Vec<usize>> {
        let mut rewriters: std::collections::HashMap<usize, Vec<usize>> =
            std::collections::HashMap::new();
        for (position, &statement) in changed.iter().enumerate() {
            if let Some(author) = self.marks.iter().position(|mark| position < mark[4]) {
                let authors = rewriters.entry(statement).or_default();
                if !authors.contains(&author) {
                    authors.push(author);
                }
            }
        }
        rewriters
    }
}

/// one `from <module> import …` statement of the module's own that a preamble line could join
struct OwnImport {
    /// the module as written, dots included, so `.mod` and `mod` are not the same target
    module: String,
    /// where to write more names: the end of the statement, which is ahead of any comment
    /// trailing it
    end: usize,
    /// the spellings it already imports, so a name it has is not added twice
    spellings: Vec<String>,
}

/// Fold each `from <module> import …` line of `imports` into a statement of `body`'s own
/// leading import block importing from the same module, and hand back the lines that found no
/// home there.
///
/// The preamble is written above the module's own imports, so a name the lowering needs and a
/// name the module imports from the same place land on two lines saying `from typing import`.
/// One line is what a reader — and the `.py` the build ships — should have. Folding also keeps
/// the output's line count unchanged, which the line table depends on: the preamble accounts
/// for the lines it adds, and a name written into a line that is already there adds none.
///
/// Only the leading block, only a statement written on one line, and only a name nothing else
/// in that block binds. Everything between the preamble's position and the statement is then an
/// import too, so moving the binding down to it cannot pass a read of the name — and cannot
/// change which binding wins, either.
///
/// And only a name the rest of the preamble does not read. The preamble is not imports alone:
/// a hoisted `class _Protocol_…(Protocol)` stands beside them and reads `Protocol` as it is
/// defined, so folding that import into the module's own line would leave the class with no
/// such name at all.
pub(crate) fn merge_into_own_imports(body: &mut String, imports: Vec<String>) -> Vec<String> {
    let at = preamble_offset(body);
    let parsed = ruff_python_parser::parse_unchecked_source(
        &body[at..],
        ruff_python_ast::PySourceType::Python,
    );
    let mut own: Vec<OwnImport> = Vec::new();
    // every name the block binds, which says whether folding a name into one of its statements
    // could change which binding the module ends up with
    let mut bound: Vec<(usize, String)> = Vec::new();
    for stmt in parsed.suite() {
        match stmt {
            Stmt::Import(import) => {
                for alias in &import.names {
                    let name = alias.asname.as_ref().map_or_else(
                        || alias.name.split('.').next().unwrap_or_default().to_owned(),
                        ToString::to_string,
                    );
                    bound.push((own.len(), name));
                }
            }
            Stmt::ImportFrom(import) => {
                let index = own.len();
                let module = format!(
                    "{}{}",
                    ".".repeat(import.level as usize),
                    import.module.as_ref().map_or("", |module| module.as_str())
                );
                let one_line = !body[at + usize::from(import.range().start())
                    ..at + usize::from(import.range().end())]
                    .contains('\n');
                let mut spellings = Vec::new();
                for alias in &import.names {
                    bound.push((
                        index,
                        alias.asname.as_ref().unwrap_or(&alias.name).to_string(),
                    ));
                    spellings.push(match &alias.asname {
                        Some(asname) => format!("{} as {asname}", alias.name),
                        None => alias.name.to_string(),
                    });
                }
                // a star import says nothing about what it binds, so nothing can be folded
                // into it or judged against it
                if one_line && !import.names.iter().any(|alias| &*alias.name == "*") {
                    own.push(OwnImport {
                        module,
                        end: at + usize::from(import.range().end()),
                        spellings,
                    });
                } else {
                    return imports;
                }
            }
            _ => break,
        }
    }

    // an import statement reads nothing, so this is what the preamble's *other* entries read
    let preamble_reads = crate::names_written(
        ruff_python_parser::parse_unchecked_source(
            &imports.join("\n"),
            ruff_python_ast::PySourceType::Python,
        )
        .suite(),
    );

    let mut left_over = Vec::new();
    let mut insertions: Vec<(usize, String)> = Vec::new();
    for line in imports {
        let Some((module, names)) = line
            .strip_prefix("from ")
            .and_then(|rest| rest.split_once(" import "))
            .filter(|(_, names)| !names.contains(['(', '*', '\n']))
        else {
            left_over.push(line);
            continue;
        };
        let Some(index) = own.iter().position(|entry| entry.module == module) else {
            left_over.push(line);
            continue;
        };
        let spellings: Vec<&str> = names.split(", ").collect();
        let elsewhere = spellings.iter().any(|spelling| {
            let bind = spelling.rsplit(" as ").next().unwrap_or(spelling);
            preamble_reads.contains(bind)
                || bound
                    .iter()
                    .any(|(owner, name)| *owner != index && name == bind)
        });
        if elsewhere {
            left_over.push(line);
            continue;
        }
        let added: Vec<&str> = spellings
            .into_iter()
            .filter(|spelling| !own[index].spellings.iter().any(|had| had == spelling))
            .collect();
        if !added.is_empty() {
            insertions.push((own[index].end, format!(", {}", added.join(", "))));
        }
    }
    // descending, so an earlier statement's offsets are still the ones the parse reported
    insertions.sort_by_key(|(end, _)| std::cmp::Reverse(*end));
    for (end, text) in insertions {
        body.insert_str(end, &text);
    }
    left_over
}

/// Coalesce repeated `from <module> import X` lines into a single
/// `from <module> import X, Y, ...` line. Preserves any non-matching
/// lines (e.g. `import foo`, `_MISSING = object()`) in their original
/// order. Names within a merged line are sorted and deduped
fn merge_from_imports(lines: Vec<String>) -> (Vec<String>, Vec<String>) {
    // preserve first-seen module order so tests that depend on specific
    // import sequence (e.g. `from typing import TypeVar, Generic` before
    // `from typing import Final`) stay stable. names within a module
    // also keep first-seen order (deduped)
    let mut groups: indexmap::IndexMap<String, Vec<String>> = indexmap::IndexMap::new();
    let mut other: Vec<String> = Vec::new();
    for line in lines {
        // an entry that spans lines is not an import line but a block — the
        // `if TYPE_CHECKING:` one, which opens with a `from typing import`. merged
        // as though the whole block were a name list, the block's own lines become
        // a "name" and every later `from typing import` lands after them, inside
        // the block: `if TYPE_CHECKING:\n    import types, overload`
        if !line.contains('\n')
            && let Some(rest) = line.strip_prefix("from ")
            && let Some((module, names)) = rest.split_once(" import ")
        {
            let entry = groups.entry(module.trim().to_owned()).or_default();
            for name in names.split(',') {
                let name = name.trim().to_owned();
                if !name.is_empty() && !entry.contains(&name) {
                    entry.push(name);
                }
            }
            continue;
        }
        other.push(line);
    }
    // `from` imports first (first-seen module order), then raw lines
    // (synthesized class defs etc.) so any class body referencing imported
    // names sees them already in scope. returned apart, so the runtime can go
    // between them
    let from_lines: Vec<String> = groups
        .into_iter()
        .map(|(module, names)| format!("from {module} import {}", names.join(", ")))
        .collect();
    (from_lines, other)
}

/// Run every registered AST pass against `source` and splice the rewritten
/// statements back into the source text. Returns a borrowed `Cow` when
/// nothing changed.
///
/// `project`, when `Some`, supplies the real project db + file so type-aware
/// passes resolve cross-module imports (e.g. an imported generic function for
/// `generic_call`). The chosen db owns the parse the type-aware passes query:
/// `inferred_type` does AST node-identity lookups, so the model and the walked
/// suite must come from one db
pub(crate) fn run_against_source<'a>(
    source: &'a str,
    written: repeated_underscore::WrittenNames,
    config: &Config,
    project: Option<(&dyn ty_python_semantic::Db, ruff_db::files::File)>,
) -> (Cow<'a, str>, Vec<String>, Vec<Option<u32>>) {
    // blank the keyword-prefix type markers — use-site variance and the
    // `literal`/`final` type modifiers — out up front; downstream passes
    // (callable, intersection) copy operand source verbatim and would leak the
    // keywords otherwise. blanking is length-preserving, so every range below
    // is valid in both the original and the blanked source
    let blanked = use_site_variance::blank(source);
    let source_ref: &str = blanked.source.as_ref();

    // the db keeps the *original* source, markers and all, so ty can answer
    // questions that depend on a use-site projection (`x is A[out int]`).
    // that's sound precisely because blanking preserves byte positions: the
    // db's parse and the blanked parse below agree on every node's range
    let sem = match project {
        Some((pdb, pfile)) => SemDb::Project(pdb, pfile),
        None => {
            let (db, file) = crate::make_in_memory_db(source);
            SemDb::Local(db, file)
        }
    };
    let (sem_db, sem_file): (&dyn ty_python_semantic::Db, ruff_db::files::File) = match &sem {
        SemDb::Project(db, f) => (*db, *f),
        SemDb::Local(db, f) => (db, *f),
    };
    let parsed_handle =
        ruff_db::parsed::parsed_module(sem_db, sem_db.program_file(sem_file).python_file(sem_db))
            .load(sem_db);
    let semantic_model =
        ty_python_semantic::SemanticModel::new(sem_db, sem_db.program_file(sem_file));

    // identity line table for the no-change early returns: stripping variance
    // is within-line, so every line still maps to itself
    if !parsed_handle.errors().is_empty() {
        let cow = blanked.stripped(source);
        let table = crate::source_map::line_table(cow.as_ref(), &[]);
        return (cow, Vec::new(), table);
    }
    let parsed = parse_unchecked_source(source_ref, PySourceType::BasedPython);
    if !parsed.errors().is_empty() {
        let cow = blanked.stripped(source);
        let table = crate::source_map::line_table(cow.as_ref(), &[]);
        return (cow, Vec::new(), table);
    }
    // the parentheses grouping an expression are visible in the tokens and
    // nowhere in the AST, so anything that needs them has to measure now — the
    // tokens go when the syntax tree is taken out of the parse
    let accessor_value_ranges =
        properties::collect_value_ranges(&parsed.syntax().body, parsed.tokens());
    // a statement an AST pass rewrote is re-emitted in the source's own indentation,
    // since the lines it passes through keep theirs
    let indentation = ruff_python_codegen::Stylist::from_tokens(parsed.tokens(), source_ref)
        .indentation()
        .clone();
    // and with the comments between the nodes it passes through, which are found in the
    // tokens rather than read off the text, where a `#` may be inside a string
    let comments = ruff_python_trivia::CommentRanges::from(parsed.tokens());
    let mut module = parsed.into_syntax();
    // what each statement was before any pass rewrote it, which is what says which
    // of its nodes a pass rewrote
    let parsed_body = module.body.clone();
    // capture each top-level statement's original source range before any
    // pass mutates the AST. AST mutations replace nodes with synthesised
    // ones whose ranges are zeroed (default `TextRange`), so the splice
    // driver can't rely on `stmt.range()` after the passes run
    let original_ranges: Vec<(usize, usize)> = module
        .body
        .iter()
        .map(|s| (usize::from(s.range().start()), usize::from(s.range().end())))
        .collect();
    let mut ctx = PassContext::default();

    // how each parameter list that repeats `_` is lowered is ty's answer, which every pass
    // that names a parameter reads through `written` — the ones walking a tree of their own
    // too, so it is read off the db's parse now, by range
    let underscore_lowerings = repeated_underscore::collect(
        parsed_handle.suite(),
        &semantic_model,
        config.min_version >= ruff_python_ast::PythonVersion::PY38,
    );
    let written = written.with_lowerings(&underscore_lowerings);

    // resolve symbolic operations in type positions (`1 + 1` → `Literal[2]`)
    // up front, from the original parse where `typeof` operands are still
    // intact for ty to read. the pass replaces each operation node and must run
    // before `typeof` lowering so a `typeof` operand is consumed here
    // `literal str` is the one use-site modifier python can spell, so it lowers
    // to `LiteralString` rather than being erased with the other markers. it has
    // to be collected from the db's own (marker-bearing) parse, since the
    // blanked copy the passes walk no longer has a `literal` keyword in it
    let literal_string_rewrites =
        literal_string::collect(parsed_handle.suite(), &semantic_model, written);

    // a static resource import has no python spelling at all, so the document it
    // names is read now — from the db's own parse, which is the one the checker
    // answered about — and written into the module in the import's place
    let static_resource_pass = static_resource::StaticResource::new(
        static_resource::collect(parsed_handle.suite(), &semantic_model),
        source_ref,
    );

    let mut symbolic_folds = symbolic_type_op::collect_symbolic_folds(
        parsed_handle.suite(),
        &semantic_model,
        written,
        config.float_literals,
        config.min_version,
    );
    let symbolic_imports = std::mem::take(&mut symbolic_folds.imports);
    ctx.claimed_type_op_ranges = symbolic_folds.claimed_ranges();
    ctx.symbolic_substitutions = symbolic_folds.substitutions();
    let symbolic_pass = symbolic_type_op::SymbolicTypeOp::new(symbolic_folds);

    // a `typeof` nested under a structural type-form (`&` / `or` / `not` / an
    // arrow) belongs to the type-expression lowerer's wide edit; the fold
    // skips those so its statement re-render doesn't drop that edit
    let typeof_skip = typeof_keyword::collect_structural_typeof_ranges(
        parsed_handle.suite(),
        &semantic_model,
        &ctx.claimed_type_op_ranges,
    );
    ctx.structural_typeof_ranges.clone_from(&typeof_skip);
    let typeof_inner =
        typeof_keyword::TypeofFold::new(typeof_skip, written.imported("ty_extensions", "TypeOf"));
    let typeof_pass = VisitorPass {
        inner: &typeof_inner,
        changed_cell: typeof_inner.changed_cell(),
        imports: vec![],
        hoist: RefCell::new(vec![]),
        text_edits: RefCell::new(vec![]),
        lowering: Some(Lowering::Typeof),
    };

    let tuple_index_pass = tuple_index::TupleIndexPass::new();

    let decorated_binding_pass = decorated_binding::DecoratedBindingPass::new(source_ref, written);

    let sentinel_inner = sentinel::Sentinel::new(written.imported("typing_extensions", "Sentinel"));
    let sentinel_pass = VisitorPass {
        inner: &sentinel_inner,
        changed_cell: sentinel_inner.changed_cell(),
        imports: vec![],
        hoist: RefCell::new(vec![]),
        text_edits: RefCell::new(vec![]),
        lowering: None,
    };

    let repeated_underscore_pass =
        repeated_underscore::RepeatedUnderscore::new(source_ref, written);

    let typed_lambda_inner = typed_lambda::TypedLambda::new(source_ref);
    let typed_lambda_pass = VisitorPass {
        inner: &typed_lambda_inner,
        changed_cell: typed_lambda_inner.changed_cell(),
        imports: vec![],
        hoist: RefCell::new(vec![]),
        text_edits: RefCell::new(vec![]),
        lowering: None,
    };

    let export_import_pass = export_import::ExportImport::new(source_ref);
    let dynamic_keyword_pass = dynamic_keyword::DynamicKeywordPass::new(written);
    let character_type_pass = character_type::CharacterTypePass::new();
    let grapheme_string_pass = grapheme_string::GraphemeStringPass::new(written);
    let type_is_pass = type_is::TypeIs::new(source_ref, written);
    let top_star_pass = top_star::TopStar::new(written);
    let identity_swap_pass = identity_swap::IdentitySwap::new(source_ref);
    let compat_pass = compat::CompatRewrite::new(source_ref, config.clone());
    let string_tag_pass = string_tag::StringTagPass::new(source_ref, config.clone());
    let dedent_string_pass = dedent_string::DedentString::new(source_ref);
    let super_keyword_pass = super_keyword::SuperKeyword::new(written);
    let postfix_await_pass = postfix_await::PostfixAwait::new(source_ref);
    let mutable_defaults_pass =
        mutable_defaults::MutableDefaultsPass::new(source_ref, written, config.is_stub);
    let unique_loop_bindings_pass =
        unique_loop_bindings::UniqueLoopBindingsPass::new(source_ref, config.unique_loop_bindings);
    let auto_quote_pass = auto_quote::AutoQuote::new(
        source_ref,
        config.min_version,
        config.inject_future_annotations,
    );
    let init_method_pass = init_method::InitMethod::new(source_ref, written, config.clone());
    let properties_pass = properties::PropertiesPass::new(
        source_ref,
        written,
        accessor_value_ranges,
        config.min_version,
    );
    let local_once_pass = local_once::LocalOncePass::new(source_ref);
    let raises_strip_pass = raises_clause::RaisesStripPass::new(source_ref);
    let return_value_use_pass = return_value_use::ReturnValueUsePass::new(
        source_ref,
        return_value_use::collect(parsed_handle.suite(), &semantic_model),
    );
    let raises_guard_pass =
        raises_clause::RaisesGuardPass::new(source_ref, config.runtime_raises_checks);
    let type_fn_pass = type_fn::TypeFnPass::new(source_ref);
    let match_type_pass = match_type::MatchTypePass::new(source_ref, written);
    let modifiers_pass = modifiers::ModifiersPass::new(source_ref, written, config);
    let main_function_pass = main_function::MainFunction::new(source_ref, written);
    let build_stamps_pass = build_stamps::BuildStampsPass::new(source_ref, config.stamps.clone());
    let empty_declarations_pass = empty_declarations::EmptyDeclarations::new();
    let overload_pass = overload::Overload::new(source_ref, written, config.is_stub);
    let decorator_keyword_pass = decorator_keyword::DecoratorKeyword::new(
        source_ref,
        written,
        config.is_stub,
        decorator_keyword::collect_return_types(
            parsed_handle.suite(),
            &semantic_model,
            config.min_version,
        ),
    );
    let unpack_pass = unpack::UnpackSyntax::new(
        written,
        config.clone(),
        unpack::collect_type_subscripts(parsed_handle.suite(), &semantic_model),
    );
    let typed_dict_literal_pass = typed_dict_literal::TypedDictLiteralPass::new(
        source_ref,
        written,
        config.clone(),
        parsed_handle.suite(),
        &semantic_model,
    );
    let just_float_pass = just_float::JustFloatPass::new(written);
    let float_const_pass = float_const::FloatConstPass::new();
    let kw_subscript_pass =
        kw_subscript::KwSubscriptPass::new(source_ref, written, config.min_version);
    let generic_call_pass = generic_call::GenericCallStripPass::new(source_ref);
    let reified_generic_pass =
        reified_generic::ReifiedGenericPass::new(source_ref, config.min_version);
    let reified_class_pass = reified_class::ReifiedClassPass::new(source_ref, config.min_version);
    let type_reification_pass = type_reification::TypeReificationPass::new(config.min_version);
    let visibility_rename_pass = visibility_rename::VisibilityRenamePass { written };
    let parametric_is_pass = parametric_is::ParametricIsPass::new(source_ref, written);
    let implicit_typing_pass = implicit_typing::ImplicitTypingPass::new();
    let inferred_annotation_pass =
        inferred_annotation::InferredAnnotationPass::new(written, config.min_version);
    let template_type_pass = template_type::TemplateTypePass { written };
    let tuple_types_pass =
        annotation::TupleLiteralTypePass::new(source_ref, written, config.clone());
    let literal_types_pass =
        literal_types::LiteralTypePass::new(source_ref, written, config.float_literals);
    let callable_pass = callable::CallableSyntaxPass::new(source_ref, written, config.clone());
    let protocol_type_pass =
        protocol_type::ProtocolTypePass::new(source_ref, written, config.clone());
    let coalesce_text_pass = coalesce::NoneCoalescePass::new(source_ref);
    let force_unwrap_pass = force_unwrap::ForceUnwrapPass::new(source_ref);
    let flexible_keyword_pass = flexible_keyword::FlexibleKeywordPass;
    let some_ctor_pass = some_ctor::SomeCtorPass::new();
    let propagate_pass = propagate::PropagatePass::new(source_ref, written);
    let none_chain_pass = none_chain::NoneChainPass::new(source_ref);
    let optional_type_pass =
        optional_type::OptionalTypePass::new(source_ref, written, config.min_version);
    let runtime_union_pass = runtime_union::RuntimeUnionPass::new(written, config.min_version);
    let generics_pass = generics::GenericPolyfillPass::new(source_ref, written, config.clone());
    let soundness_pass = soundness::SoundnessPass::new(source_ref, written, config);
    let checked_cast_pass = checked_cast::CheckedCastPass { written };
    let module_api_pass = module_api::ModuleApiPass::new(source_ref);
    let trailing_lambda_pass = trailing_lambda::TrailingLambdaPass::new(source_ref);
    let if_let_pass = if_let::IfLetPass::new(source_ref);
    let class_pattern_star_pass = class_pattern_star::ClassPatternStarPass::new(source_ref);
    let destructure_pass = destructure::DestructurePass::new(source_ref);
    let statement_expression_pass = statement_expression::StatementExpressionPass::new(source_ref);
    let context_params_pass = context_params::ContextParamsPass::new(source_ref, written);
    let extension_block_pass =
        extension::ExtensionBlockPass::new(source_ref, written, config.is_stub);
    let extension_call_pass = extension::ExtensionCallPass { written };
    let witness_dispatch_pass = conformance::WitnessDispatchPass;
    let conversion_pass = conversion::ConversionPass::new(source_ref);
    let implicit_receiver_pass = implicit_receiver::ImplicitReceiverPass { written };
    let django_lookup_pass = django_lookup::DjangoLookupPass;
    let frameworks_pass = frameworks::FrameworksPass::new(source_ref);
    let variance_pass = decl_site_variance::VarianceStripPass::new(source_ref);
    let anon_named_tuple_pass =
        anon_named_tuple::AnonNamedTuplePass::new(source_ref, written, config.clone());

    // Order matters: passes that read source ranges via `text_edits` mode
    // must run BEFORE passes that mutate the AST (which zero source ranges
    // on synthesised nodes). All text-edit-emitting passes here read AST
    // node ranges to compute their edits; once another pass replaces an
    // Expr wholesale, its range is `TextRange::default()` and source lookups
    // are invalid.
    let passes: &[&dyn AstPass] = &[
        // a statement expression moves its enclosing statement's assignment
        // below a suite; every other lowering inside that suite composes through
        // the passthrough spans it emits, so it goes first
        &statement_expression_pass,
        // destructuring: the `let` statement, patterns in binding positions, and
        // the `and` pattern. Like `if let` it replaces headers only, so bodies
        // keep their source bytes and the lowerings inside them compose
        &destructure_pass,
        // text-edit-emitting passes first (read source ranges).
        // type_is rewrites a `-> a is T` return guard into `TypeIs[T]`, which
        // claims the whole guard; `parametric_is` would otherwise lower the
        // same `is` pair inside it
        &type_is_pass,
        // `from x export y` → `from x import y as y`: two source edits inside
        // an import statement, independent of every other pass
        &export_import_pass,
        &top_star_pass,
        &identity_swap_pass,
        &compat_pass,
        // a custom string tag wraps a template literal whose interpolations may
        // themselves lower; its template-edit passes interpolation source
        // through as `Src` fragments so those inner edits still compose
        &string_tag_pass,
        &dedent_string_pass,
        &super_keyword_pass,
        &postfix_await_pass,
        // strip `local` / `once` parameter modifiers (source-span deletions,
        // like init_method's `let` handling — must read ranges before any
        // AST-mutation pass zeroes them)
        &local_once_pass,
        // delete `raises` clauses (a source-span deletion, like `local` / `once`
        // — must read ranges before any AST-mutation pass zeroes them)
        &raises_strip_pass,
        // delete the return-value markers. it also strips them from the AST, so
        // it has to run before any pass re-renders a statement one sits on
        &return_value_use_pass,
        // erase `type def` declarations; their applications were already folded to
        // the resolved type by the symbolic pass above
        &type_fn_pass,
        // erase `implements` declarations — a whole-line source deletion, so it
        // has to read ranges before any AST-mutation pass zeroes them
        &module_api_pass,
        // replace a match type's `case` blocks with a runtime value, and strip
        // `TypeVarTuple` bounds wherever they appear
        &match_type_pass,
        // a decorator above a binding: erase the decorator lines and wrap the
        // value. Before `modifiers`, whose prefix rewrite (`let a = ` →
        // `a: Final = `) sits immediately after the lines erased here, and early
        // enough that an equal-span rewrite of the same value from a later pass
        // is materialized inside the wrap rather than dropped against it
        &decorated_binding_pass,
        &modifiers_pass,
        // after modifiers so the entry-point guard follows any `__all__` it
        // emits, and before the AST-mutation passes so `main`'s decorator
        // ranges are still valid for the `private` check
        &main_function_pass,
        // replaces a whole `build:` block with the class it lowers to, reading
        // the block's own source for each default — so it has to run while those
        // ranges still mean something, ahead of every mutation pass
        &build_stamps_pass,
        &empty_declarations_pass,
        &overload_pass,
        &decorator_keyword_pass,
        &unpack_pass,
        &typed_dict_literal_pass,
        // AST-mutation passes second (may zero node ranges).
        // symbolic_type_op replaces whole operation nodes (consuming any
        // `typeof` operand) and reads original source ranges, so it must run
        // first among the mutation passes — before `typeof` and before any
        // pass that zeroes ranges
        &symbolic_pass,
        &typeof_pass,
        &sentinel_pass,
        &repeated_underscore_pass,
        &typed_lambda_pass,
        // a static resource import is replaced whole by the document it names,
        // as an edit of the source
        &static_resource_pass,
    ];
    let mut authorship = Authorship::default();
    for pass in passes {
        if config.is_stub && pass.runtime_only() {
            continue;
        }
        pass.run(&mut module, &mut ctx);
        authorship.finished(
            Author {
                name: pass.name(),
                lowering: pass.lowering(),
                subsumes: pass.subsumes(),
            },
            &ctx,
        );
    }

    // type-aware passes: operate on the salsa-owned parsed module (so
    // semantic queries hit the right AST nodes), emit text_edits / imports
    let type_aware: &[&dyn TypeAwarePass] = &[
        // framework gates only push errors (no edits), so their position is
        // order-independent; first, so a hard incompatibility surfaces
        // before any edit-conflict noise
        &frameworks_pass,
        // forward references are quoted with one wrapper template per span, so
        // the lowerings inside an annotation (an arrow, a `T?`) land between the
        // quotes wherever they come in the list
        &auto_quote_pass,
        // the `raises` runtime guard is a decorator inserted at the start of the
        // `def` line, so it composes with every edit inside the signature and
        // body (the clause deletion among them)
        &raises_guard_pass,
        // `init(...)` shorthand: rewrite to `def __init__`, strip `let`, and
        // synthesize `self.<name>: <ann> = <name>`. type-aware because the
        // synthesized annotation is fresh output that must reproduce whatever
        // lowering the parameter's own annotation gets (a callable arrow, a
        // `T?`, a bare `float`); the imports / hoisted classes those need are
        // requested by the sibling passes' visit of the same parameter
        &init_method_pass,
        // property accessor blocks: replace the whole `var`/`let` + `get`/`set`
        // construct with the python `@property` members the parser already
        // synthesized. it claims the construct span as one template, so it runs
        // among the early template-claiming passes; type positions and the
        // backing initialiser pass through as `Src` and still compose
        &properties_pass,
        // soundness wraps whole gated expressions in `_soundness_check(...)`
        // template edits; it runs first so an equal-span template from a
        // later pass (e.g. coalesce on a wrapped iterable) is claimed and
        // materialized inside the check rather than dropping it
        &soundness_pass,
        // a `private` method is reached by its mangled name; the edit replaces
        // the attribute identifier alone, so it composes with any rewrite of the
        // receiver it is read from
        &visibility_rename_pass,
        // checked cast wraps `<value> cast? <type>` in `_checked_cast(...)`; its
        // template passes value + type through as `Src`, so lowerings inside
        // them (a `??` value, a `T?` type) still compose
        &checked_cast_pass,
        // trailing lambda blocks re-emit a whole statement as a def + call
        // template; the suite and the called expression pass through as `Src`,
        // so lowerings inside them (including nested trailing lambdas) are
        // claimed and materialized in place
        &trailing_lambda_pass,
        // fill in a class pattern's `*_` with the wildcards it stood for. the
        // edit covers the `*_` bytes alone, so it composes inside the header
        // templates the two pattern lowerings below build
        &class_pattern_star_pass,
        // `if let <pattern> := <subject>:` chains flatten onto a selector
        // variable. only the clause headers are replaced, so every body keeps
        // its source bytes and the lowerings inside them compose
        &if_let_pass,
        // context parameters: strip `context` prefixes, lower `context NAME =`
        // declarations, and append the resolved implicit arguments before each
        // call's closing paren. single insertions, so they compose inside any
        // wrapping template's `Src` spans (including the trailing-lambda one)
        &context_params_pass,
        // `extension` blocks lower to module-level backing functions; member
        // bodies pass through as `Src` spans so lowerings inside them (and
        // the call rewrites below) still compose
        &extension_block_pass,
        // type-directed rewrite of attribute accesses ty resolved to
        // extension members (`xs.second()` → `_by_ext__list__second(xs)`)
        &extension_call_pass,
        // a protocol *requirement* read off an interface-typed receiver goes
        // through the witness table its conformance registered, since the value
        // may carry no such attribute of its own. disjoint from the extension
        // rewrite above, which only fires where normal member lookup found
        // nothing — a requirement is a member of the interface
        &witness_dispatch_pass,
        // every conversion site the checker accepted wraps its value. reads
        // source ranges, so it runs alongside the extension passes and before
        // the AST-mutating ones
        &conversion_pass,
        // implicit receivers: `x.fn()` → `fn(x)` for a receiver callable in
        // scope, and a trailing lambda block's `self` / unqualified receiver
        // members → its receiver parameter. same shape as the extension rewrite
        // above, which wins when both could apply
        &implicit_receiver_pass,
        // django lookups written as expressions (`filter(author.name == "x")`)
        // become keyword arguments. the argument is replaced whole, with the
        // value passing through as `Src`, so lowerings inside it still compose;
        // disjoint from the call rewrites above, which target the callee
        &django_lookup_pass,
        &dynamic_keyword_pass,
        // import-only companion to the ty-side implicit `Character` resolution;
        // emits no text edits, so ordering among the type passes is free
        &character_type_pass,
        &just_float_pass,
        &float_const_pass,
        &kw_subscript_pass,
        // reified generics wrap `def f[T]` (value-position `T`) in `@generic`;
        // must precede generic_call so the call-site strip skips the wrapped
        // function's specialized calls (they route through `generic.__getitem__`)
        &reified_generic_pass,
        // reified class generics decorate `class A[T]` (value-position `T`) with
        // `@generic_class` and bind each read from its method's receiver. it
        // must precede type_reification, whose injected `A[int](…)` is what
        // reaches the specializer the decorator installs
        &reified_class_pass,
        &generic_call_pass,
        // type reification: bare generic constructor calls get their solved
        // specialization (`A(1)` → `A[int](1)`) and collection literals their
        // inferred element types (`[1, 2]` → `list[int]([1, 2])`). disjoint
        // from the two passes above — they handle *function* callees, this one
        // class callees and displays
        &type_reification_pass,
        // parametric type tests (`x is list[int]`): identity_swap leaves
        // keyword-form `is` pairs with a subscripted rhs for this pass, which
        // resolves them rust-style from static types (fold / reified-cell
        // token equality / witness probe / unchecked runtime probe)
        &parametric_is_pass,
        &implicit_typing_pass,
        // synthesize declared types for bare class-body assignments
        // (`class A: a = 1` → `a: int = 1`); a zero-width insertion at the
        // target name, disjoint from the value-position lowerings above
        &inferred_annotation_pass,
        // an f-string in a type position replaces its whole span with the
        // python spelling of the type ty resolved it to. it runs before the
        // type-position passes below so its wide edit claims the holes, whose
        // own lowerings would otherwise be emitted for source that is gone
        &template_type_pass,
        &tuple_types_pass,
        &literal_types_pass,
        &callable_pass,
        // inline protocols hoist to a synthesized `Protocol` class; the wide
        // replacement covers the whole `protocol(...)` span, so it must follow
        // `callable` — whose visit of the same member types emits the imports
        // and `_Callable_*` classes our re-rendered copies name
        &protocol_type_pass,
        // `T?` → `T | None`; a type-position edit, disjoint from the
        // value-position `??` / `?.` lowerings below
        &optional_type_pass,
        // a PEP 604 union the runtime will evaluate (`isinstance(x, int | str)`)
        // is spelled the way the target can. its template covers the whole
        // union, and the sort puts a wider replacement first, so the lowerings
        // inside each arm are materialized rather than dropped
        &runtime_union_pass,
        // coalesce sees `?.` LHS via source ranges; must run BEFORE
        // none_chain so its wider `??` edit wins over none_chain's narrow
        // `?.` edit when both target the same span
        &coalesce_text_pass,
        // `expr!` → `_force_unwrap(expr)`; narrow insert/replace edits that compose
        // with sibling operator lowerings inside the operand
        &force_unwrap_pass,
        // `f(a.b=1)` → `f(**{"a.b": 1})`; the argument's value passes through as
        // `Src`, so lowerings inside it still compose
        &flexible_keyword_pass,
        // `expr.N` → `expr[N]`; a narrow replacement of the `.N` bytes only
        &tuple_index_pass,
        // grapheme string surface (`s.character_count` → `len(_by_graphemes(s))`,
        // …); receiver spans pass through as `Src` fragments, so sibling
        // lowerings inside compose
        &grapheme_string_pass,
        // mutable defaults → `_MISSING` sentinel swap + body-prologue guard;
        // narrow edits, so the function body's own lowerings still apply
        &mutable_defaults_pass,
        // per-iteration loop bindings: a closure made in a loop body is applied
        // to the loop's values through a wrapper (or, for a `def`, gets a
        // closure-rebinding decorator). the closure passes through as `Src`, so
        // every lowering inside it still composes. after reified generics, so
        // the `@generic` line stays above the rebind and receives the rebuilt
        // function
        &unique_loop_bindings_pass,
        // `Some(x)` → `Optional(x)`; a narrow identifier rename
        &some_ctor_pass,
        // `expr^` → guard hoisted before the enclosing statement + unwrapped value
        &propagate_pass,
        &none_chain_pass,
        // generics emits wide replacements covering whole type-params
        // headers; variance's narrow def-site deletion gets dropped by
        // first-wins dedup when generics fires (3.10), survives when
        // generics doesn't (3.12+ native PEP 695)
        &generics_pass,
        &variance_pass,
        // anon_named_tuple must run BEFORE tuple_types so its outer-region
        // edits win when isolation conflicts arise — but tuple_types is
        // already earlier in this list. The cleanup-loop in lib.rs catches
        // anon-NT spans generics polyfill leaked verbatim into class headers
        &anon_named_tuple_pass,
    ];
    for pass in type_aware {
        if config.is_stub && pass.runtime_only() {
            continue;
        }
        pass.run(parsed_handle.suite(), &semantic_model, &mut ctx);
        authorship.finished(
            Author {
                name: pass.name(),
                lowering: pass.lowering(),
                subsumes: pass.subsumes(),
            },
            &ctx,
        );
    }

    // collect import requests the inner passes raised at the end of their run
    if typeof_inner.ever_changed() {
        ctx.required_imports
            .push(written.import_from("ty_extensions", &["TypeOf"]));
    }
    // symbolic folds that produced a `Literal[..]` or an `Any` (`dynamic + 1`) need the import
    ctx.required_imports.extend(symbolic_imports);
    // typed lambdas are removed as source deletions so the statement around
    // them is never re-rendered (see `typed_lambda`); collect them here
    ctx.text_edits.extend(typed_lambda_inner.take_edits());
    authorship.finished(Author::driver(typed_lambda_pass.name(), None), &ctx);
    if sentinel_inner.ever_changed() {
        ctx.required_imports
            .push(written.import_from("typing_extensions", &["Sentinel"]));
    }

    // collapse the padding `blank` left behind. these are ordinary edits, so a
    // statement an AST pass re-rendered (which never had the marker in its
    // AST) and a wider template edit (which materializes them inside its `Src`
    // spans) both come out clean; only a wider *plain* text edit keeps the
    // padding, which is valid Python either way
    for range in &blanked.ranges {
        // a marker rewritten to `LiteralString` has its whole range replaced
        // below; emitting the keyword collapse too would leave two plain text
        // edits racing for the same start offset
        if literal_string_rewrites.covers(*range) {
            continue;
        }
        ctx.text_edits
            .push((*range, use_site_variance::collapsed_to(source_ref, *range)));
    }
    authorship.finished(
        Author::driver("keyword blanking", Some(Lowering::KeywordPadding)),
        &ctx,
    );
    for (range, replacement) in literal_string_rewrites.edits() {
        ctx.text_edits.push((range, replacement));
    }
    authorship.finished(Author::driver("literal string", None), &ctx);
    if literal_string_rewrites.needs_import {
        ctx.required_imports
            .push(written.import_from("typing", &["LiteralString"]));
    }

    // the last thing written into the tree: a soundness check made in a statement an
    // `AstPass` rewrote, which the re-render reads rather than the edit list. it happens
    // here, ahead of the runtime block below, because the two rewrites that answer for
    // what a program reads back read the finished tree and can ask for a helper of their
    // own
    for (idx, checks) in std::mem::take(&mut ctx.rerendered_checks) {
        if let Some(stmt) = module.body.get_mut(idx)
            && let Err(error) = super::soundness::place_in_rerendered(stmt, checks)
        {
            ctx.errors.push(error);
        }
    }

    // which t-string fields a pass replaced in the syntax tree, read before the `=` field
    // rewrite below moves any of them: the tree still holds one field per field the
    // author wrote, which is what matches the two up
    let fields_replaced_in_tree =
        template_expression::replaced_in_tree(&module, &parsed_body, &ctx.changed);

    // every edit that puts something other than what it covers at the span it covers —
    // which is to say, every lowering that actually lowered something. the two rewrites
    // below read this to decide which replacement fields report a text that is no longer
    // the author's
    //
    // an edit that writes back what it covers is left out. the keyword blanking makes a
    // plain one for every marker it collapses, and a type test against `None` on an
    // optional makes a template one — `a is not None` is the python spelling of
    // `a is None?`'s negation and of what the author wrote. acting on either would print
    // the same text out of a longer f-string, which is how a `.py` file's
    // `f"{(a if a is not None else b)=}"` stopped surviving a round trip
    let rewriting: Vec<TextRange> = ctx
        .text_edits
        .iter()
        .filter(|(range, replacement)| {
            source_ref.get(usize::from(range.start())..usize::from(range.end()))
                != Some(replacement.as_str())
        })
        .map(|(range, _)| *range)
        .chain(
            ctx.template_edits
                .iter()
                .filter(|(range, frags)| rewrites_covered_source(*range, frags, source_ref))
                .map(|(range, _)| *range),
        )
        .chain(
            ctx.relocating_edits
                .iter()
                .filter(|(range, frags)| rewrites_covered_source(*range, frags, source_ref))
                .map(|(range, _)| *range),
        )
        .chain(
            ctx.statement_inserts
                .iter()
                .map(|(at, _)| TextRange::empty(*at)),
        )
        .collect();
    // a t-string hands a reader the source text of every one of its fields, not only a
    // `=` one, so a field any lowering reached has to report what the author wrote. which
    // templates those are is known only now, from the finished edit list, for the same
    // reason the `=` fields above are
    let template_wraps = template_expression::claim(
        source_ref,
        parsed_handle.suite(),
        &rewriting,
        &fields_replaced_in_tree,
        config.min_version >= ruff_python_ast::PythonVersion::PY314,
    );

    // a synthesized annotation may name a class the source never imported. the
    // import goes under `if TYPE_CHECKING:` as one block: every such annotation
    // is a string at runtime (the lowering always emits the `__future__` import),
    // so only a checker ever reads the name, and a real import would give the
    // output an import edge — and a possible cycle — the source never had
    if !ctx.type_only_imports.is_empty() {
        // the one typing name a lowering writes under its own name whatever the module
        // binds: the block reads it where it binds it, ahead of anything the module runs
        let mut block = String::from("from typing import TYPE_CHECKING\nif TYPE_CHECKING:");
        for line in std::mem::take(&mut ctx.type_only_imports) {
            block.push_str("\n    ");
            block.push_str(&line);
        }
        ctx.required_imports.push(block);
    }

    ctx.required_imports.sort();
    ctx.required_imports.dedup();
    let (imports, definitions) = merge_from_imports(std::mem::take(&mut ctx.required_imports));
    ctx.required_imports = imports;
    // the runtime goes after the imports and ahead of everything else: it needs
    // nothing from the module, and a synthesized class may name one of its
    // helpers where it is evaluated at once — `Optional` in the annotation of a
    // `NamedTuple` field. never sorted: set-up code follows its definition
    if template_wraps
        .iter()
        .any(template_expression::Wrap::rebuilds)
    {
        ctx.runtime.insert(crate::runtime::TEMPLATE_TEXT);
    }
    if !ctx.runtime.is_empty() {
        let helpers = ctx.runtime.iter().copied();
        match config.runtime_module.as_deref() {
            Some(module) => ctx
                .required_imports
                .push(crate::runtime::import_line(module, helpers)),
            None => ctx.required_imports.extend(crate::runtime::inline(helpers)),
        }
    }
    ctx.required_imports.extend(definitions);
    // who rewrote each statement, read before the list is put in order
    let rewritten_by = authorship.rewriters(&ctx.changed);
    ctx.changed.sort_unstable();
    ctx.changed.dedup();

    if ctx.changed.is_empty()
        && ctx.required_imports.is_empty()
        && ctx.hoisted.is_empty()
        && ctx.text_edits.is_empty()
        && ctx.template_edits.is_empty()
        && ctx.statement_inserts.is_empty()
        && ctx.epilogue.is_empty()
    {
        let cow = blanked.stripped(source);
        let table = crate::source_map::line_table(cow.as_ref(), &[]);
        return (cow, ctx.errors, table);
    }

    // splice changed statements back into the source string. process highest
    // index first so byte offsets in unmodified prefixes stay valid through
    // the loop. hoisted statements are emitted as text just before the
    // splice for their target idx
    let mut hoisted_by_idx: std::collections::BTreeMap<usize, Vec<Stmt>> =
        std::collections::BTreeMap::new();
    for (idx, stmt) in std::mem::take(&mut ctx.hoisted) {
        hoisted_by_idx.entry(idx).or_default().push(stmt);
    }

    // an f-string `=` field prints the source text between its braces, so a field whose
    // expression a pass replaced in the syntax tree would print that replacement rather
    // than what the author wrote. the field is taken apart here, once the tree is the one
    // the re-render will read, and the fields it claimed are kept so the edit-driven half
    // below leaves them alone
    let (debug_fields_claimed, debug_field_errors) =
        debug_field::rewrite_changed(&mut module, &parsed_body, &ctx.changed);
    ctx.errors.extend(debug_field_errors);

    let mut edited: Vec<TextRange> = ctx
        .text_edits
        .iter()
        .map(|(range, _)| *range)
        .chain(ctx.template_edits.iter().map(|(range, _)| *range))
        .chain(ctx.relocating_edits.iter().map(|(range, _)| *range))
        .chain(
            ctx.statement_inserts
                .iter()
                .map(|(at, _)| TextRange::empty(*at)),
        )
        .collect();
    // the other half of the `=` field rewrite: a field whose expression one of the edits
    // above rewrites is taken apart over the whole field, so the author's own text is
    // printed beside the value rather than in place of it. it comes last because it is
    // the finished edit list that says which fields those are
    let (debug_field_edits, debug_field_errors) =
        debug_field::claim_lowered(parsed_handle.suite(), &rewriting, &debug_fields_claimed);
    ctx.errors.extend(debug_field_errors);
    edited.extend(debug_field_edits.iter().map(|(range, _)| *range));
    ctx.template_edits.extend(debug_field_edits);
    authorship.finished(Author::driver("`=` field rewrite", None), &ctx);

    if !template_wraps.is_empty() {
        // a template in a statement the driver re-renders is rebuilt in the tree, which is
        // what that re-render reads. everywhere else the literal stays in the source and
        // the rebuild is an edit over it, so a lowering inside a field still applies
        let rerendered_spans: Vec<TextRange> = ctx
            .changed
            .iter()
            .filter_map(|&idx| original_ranges.get(idx))
            .map(|&(start, end)| {
                TextRange::new(
                    TextSize::try_from(start).unwrap_or_default(),
                    TextSize::try_from(end).unwrap_or_default(),
                )
            })
            .collect();
        // a string tag's literal stays in the source: its call is written there, as an edit
        let (in_tree, in_source): (Vec<_>, Vec<_>) = template_wraps.into_iter().partition(|wrap| {
            !wrap.tagged
                && rerendered_spans
                    .iter()
                    .any(|span| span.contains_range(wrap.range))
        });
        template_expression::rewrite_changed(&mut module, &ctx.changed, &in_tree);
        for wrap in in_source {
            let fragments = wrap.fragments();
            edited.push(wrap.range);
            ctx.template_edits.push((wrap.range, fragments));
        }
    }
    authorship.finished(Author::driver("t-string field rewrite", None), &ctx);

    let rerendering =
        super::rerender::Rerendering::new(source_ref, &comments, &edited, &indentation);
    let mut rerendered: Vec<(TextRange, Vec<Fragment>)> = Vec::new();
    let mut rerendered_by: Vec<Vec<usize>> = Vec::new();
    for &idx in &ctx.changed {
        let (start, end) = original_ranges[idx];
        let range = TextRange::new(
            TextSize::try_from(start).unwrap_or_default(),
            TextSize::try_from(end).unwrap_or_default(),
        );
        let edits = rerendering.edits(&parsed_body[idx], &module.body[idx], range);
        let rewriters = rewritten_by.get(&idx).cloned().unwrap_or_default();
        rerendered_by.extend(std::iter::repeat_n(rewriters, edits.len()));
        rerendered.extend(edits);
    }

    let mut edits: Vec<(usize, usize, Replacement)> = Vec::new();
    for (idx, hoists) in std::mem::take(&mut hoisted_by_idx) {
        let (start, _) = original_ranges[idx];
        let line_indent = {
            let prefix = &source_ref[..start];
            let line_start = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
            &source_ref[line_start..start]
        }
        .to_owned();
        // inserted ahead of the statement, whose source bytes (and every edit inside
        // them) stay where they are
        let mut block = String::new();
        for h in hoists {
            let rendered = render_stmt(&h).trim_end_matches('\n').to_owned();
            block.push_str(&rendered);
            block.push('\n');
            block.push_str(&line_indent);
        }
        edits.push((start, start, Replacement::generated(&block, start)));
    }
    // ruff-style first-wins dedup for sub-statement edits. sort by start; skip
    // any edit whose start is before the running cursor (overlaps a prior
    // edit) or which collides with a whole-statement splice. zero-width
    // insertions (start == end) at the cursor are allowed — they consume no
    // source bytes so multiple insertions + a deletion at the same position
    // can compose. a plain-text edit wins over anything nested inside it; a
    // template edit instead *materializes* nested edits within its `Src`
    // passthrough spans, so wide rewrites compose with inner lowerings
    // who wrote each edit, in the order they are chained below
    let origins: Vec<Vec<usize>> = [
        (EditList::Text, ctx.text_edits.len()),
        (EditList::Template, ctx.template_edits.len()),
        (EditList::Statement, ctx.statement_inserts.len()),
        (EditList::Relocating, ctx.relocating_edits.len()),
    ]
    .into_iter()
    .flat_map(|(list, len)| (0..len).map(move |index| (list, index)))
    .map(|(list, index)| authorship.author_of(list, index).into_iter().collect())
    .chain(rerendered_by)
    .collect();
    let sub_edits: Vec<(usize, usize, SubPatch)> = ctx
        .text_edits
        .into_iter()
        .map(|(r, s)| {
            (
                usize::from(r.start()),
                usize::from(r.end()),
                SubPatch::Text(s),
            )
        })
        .chain(ctx.template_edits.into_iter().map(|(r, frags)| {
            (
                usize::from(r.start()),
                usize::from(r.end()),
                SubPatch::Template(frags),
            )
        }))
        .chain(ctx.statement_inserts.into_iter().map(|(at, frags)| {
            let at = usize::from(at);
            (at, at, SubPatch::Statement(frags))
        }))
        .chain(ctx.relocating_edits.into_iter().map(|(r, frags)| {
            (
                usize::from(r.start()),
                usize::from(r.end()),
                SubPatch::Relocating(frags),
            )
        }))
        .chain(rerendered.iter().map(|(r, frags)| {
            (
                usize::from(r.start()),
                usize::from(r.end()),
                SubPatch::Rerendered(frags.clone()),
            )
        }))
        .collect();
    let (spliced, lost) = splice(source_ref, sub_edits, origins, &authorship.authors);
    edits.extend(spliced);
    ctx.errors.extend(lost);
    // line table for the spliced body, built from the ascending edit list
    // before the descending application sort consumes it. generated lines from
    // the import prefix (top) and epilogue (bottom) have no source origin
    let mut body_edits = edits.clone();
    body_edits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let body_table = crate::source_map::line_table(source_ref, &body_edits);

    // sort by start descending so prefix offsets stay valid through replace_range.
    // tie-break by end descending so wider edits (deletions) are applied before
    // zero-width insertions sharing the same start — otherwise the insertion's
    // text would land inside the deletion's range and be wiped on the next pass
    edits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    let mut out = source_ref.to_owned();
    for (start, end, repl) in edits {
        out.replace_range(start..end, repl.text());
    }
    // a builtin a lowering wrote where the module binds its name to something else is read
    // under a name of the lowering's own, bound here, ahead of everything that reads it
    let everything_written = std::iter::once(out.as_str())
        .chain(ctx.required_imports.iter().map(String::as_str))
        .chain(ctx.epilogue.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let builtins = written.builtin_imports(&everything_written, config.min_version.minor);
    ctx.required_imports.splice(0..0, builtins);
    // a name the module already imports from the same place goes on its line rather than on
    // one of ours. done here and not against `required_imports` alone, because the statement
    // it joins is the module's own
    if !ctx.required_imports.is_empty() {
        ctx.required_imports =
            merge_into_own_imports(&mut out, std::mem::take(&mut ctx.required_imports));
    }
    // an entry may be multi-line (runtime helper defs), so the table prefix
    // counts the lines each entry emits, not the entries themselves
    let prefix_lines: usize = ctx
        .required_imports
        .iter()
        .map(|imp| crate::newline_count(imp) + 1)
        .sum();
    let mut preamble_end = 0usize;
    // lines of `out` the preamble is spliced *after*, which therefore keep their
    // own source mapping ahead of the preamble's `None`s
    let mut kept_before_preamble = 0usize;
    if !ctx.required_imports.is_empty() {
        let mut prefix = String::new();
        for imp in &ctx.required_imports {
            prefix.push_str(imp);
            prefix.push('\n');
        }
        // a BOM, the module docstring and a `from __future__ import …` line all
        // have to stay first — the BOM is only a BOM at offset 0, a docstring
        // pushed down by the preamble stops being one (leaving the built
        // module's `__doc__` empty), and the future import is a syntax error
        // anywhere else. so splice required imports in after all three
        let at = preamble_offset(&out);
        kept_before_preamble = crate::newline_count(&out[..at]);
        out.insert_str(at, &prefix);
        preamble_end = at + prefix.len();
    }
    let mut table: Vec<Option<u32>> = Vec::with_capacity(prefix_lines + body_table.len());
    let kept = kept_before_preamble.min(body_table.len());
    table.extend(body_table[..kept].iter().copied());
    table.extend(std::iter::repeat_n(None, prefix_lines));
    table.extend(body_table[kept..].iter().copied());
    if !ctx.epilogue.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        for line in &ctx.epilogue {
            out.push_str(line);
            out.push('\n');
        }
        table.extend(std::iter::repeat_n(None, ctx.epilogue.len()));
    }
    // normalise trailing newline. AST-mutation passes splice rendered
    // multi-line statements that may bring their own internal newlines;
    // EOF without `\n` after such a splice looks awkward. for pure
    // sub-statement text-edit changes we preserve the source's exact
    // end-of-file shape (matters for tests like `final a = 1` with no
    // trailing newline)
    let did_render_stmt = !ctx.changed.is_empty();
    let needs_trailing_nl = source_ref.ends_with('\n') || did_render_stmt;
    if needs_trailing_nl && !out.ends_with('\n') {
        out.push('\n');
    }
    // extension backing functions are lowered in place (so their bodies keep
    // their source ranges for sibling-pass composition); hoist them to the
    // module top now that lowering is done, so a member called before its
    // block's position still resolves
    let (out, table) = extension::hoist_backing_functions(out, table, preamble_end);
    (Cow::Owned(out), ctx.errors, table)
}

#[cfg(test)]
mod driver_tests {
    use super::*;
    use crate::Config;

    /// what [`splice`] reports lost of `edit`, made against `source`, once its first
    /// statement is rewritten by `rewrite` and re-emitted
    fn lost(source: &str, rewrite: impl FnOnce(&mut Stmt), edit: (TextRange, &str)) -> Vec<String> {
        let parsed = ruff_python_parser::parse_module(source).expect("the source parses");
        let comments = ruff_python_trivia::CommentRanges::from(parsed.tokens());
        let module = parsed.into_syntax();
        let original = &module.body[0];
        let mut rewritten = original.clone();
        rewrite(&mut rewritten);
        let indentation = Indentation::default();
        let edited = [edit.0];
        let rerendered =
            super::super::rerender::Rerendering::new(source, &comments, &edited, &indentation)
                .edits(original, &rewritten, original.range());
        let mut sub_edits: Vec<(usize, usize, SubPatch)> = rerendered
            .into_iter()
            .map(|(range, frags)| {
                (
                    usize::from(range.start()),
                    usize::from(range.end()),
                    SubPatch::Rerendered(frags),
                )
            })
            .collect();
        sub_edits.push((
            usize::from(edit.0.start()),
            usize::from(edit.0.end()),
            SubPatch::Text(edit.1.to_owned()),
        ));
        let origins = vec![Vec::new(); sub_edits.len()];
        splice(source, sub_edits, origins, &[]).1
    }

    /// the call of `stmt`, an assignment of one
    fn assigned_call(stmt: &mut Stmt) -> &mut ruff_python_ast::ExprCall {
        let Stmt::Assign(assign) = stmt else {
            panic!("an assignment")
        };
        let Expr::Call(call) = assign.value.as_mut() else {
            panic!("a call")
        };
        call
    }

    /// a pass that rebuilds an argument prints it from the syntax tree, where `'s'` is
    /// spelled `"s"`. the edit a lowering made of `'s'` is in none of the source the
    /// statement passes through, and is refused — looking for its construct again in
    /// the output, which does not spell it the same way, would have let it go
    #[test]
    fn an_edit_in_a_construct_a_rewrite_respells_is_refused() {
        let source = "x = f('s')\n";
        let argument = TextRange::new(TextSize::new(6), TextSize::new(9));
        let errors = lost(
            source,
            |stmt| {
                let mut rebuilt = ruff_python_parser::parse_expression("\"s\"")
                    .expect("an expression")
                    .into_expr();
                super::super::rerender::forget_expr_ranges(&mut rebuilt);
                assigned_call(stmt).arguments.args[0] = rebuilt;
            },
            (argument, "S"),
        );
        assert_eq!(
            errors,
            [
                "transform conflict: an edit of `'s'` lands inside a node an AST pass rewrote and prints again, so the lowering would be lost"
            ]
        );
    }

    /// an insertion inside a node a pass rewrote lands only in source the node passes
    /// through. the callee is printed from the syntax tree, and the insertion made
    /// after the name it replaced has nowhere to go
    #[test]
    fn an_insertion_with_no_source_to_land_in_is_refused() {
        let source = "x = f(a)\n";
        let errors = lost(
            source,
            |stmt| {
                let mut callee = ruff_python_parser::parse_expression("g")
                    .expect("an expression")
                    .into_expr();
                super::super::rerender::forget_expr_ranges(&mut callee);
                *assigned_call(stmt).func = callee;
            },
            (TextRange::empty(TextSize::new(5)), "_inserted"),
        );
        assert_eq!(
            errors,
            [
                "transform conflict: text inserted at byte 5 lands inside a node an AST pass rewrote and prints again, so the lowering would be lost"
            ]
        );
    }

    /// an edit inside a node the rewritten statement kept is passed through with it
    #[test]
    fn an_edit_in_source_a_rewrite_passes_through_is_applied() {
        let source = "x = f(a)\n";
        let errors = lost(
            source,
            |stmt| {
                let mut callee = ruff_python_parser::parse_expression("g")
                    .expect("an expression")
                    .into_expr();
                super::super::rerender::forget_expr_ranges(&mut callee);
                *assigned_call(stmt).func = callee;
            },
            (TextRange::new(TextSize::new(6), TextSize::new(7)), "b"),
        );
        assert_eq!(errors, Vec::<String>::new());
    }

    /// what [`splice`] reports lost of an edit by `inserter` of `", /"` after `b` in
    /// `def f(a, b): ...`, once a template by `writer` spells `a, b)` itself
    fn lost_in_a_written_parameter_list(writer: Author, inserter: Author) -> Vec<String> {
        let source = "def f(a, b): ...\n";
        let parameters = TextRange::new(TextSize::new(6), TextSize::new(11));
        let sub_edits = vec![
            (
                6,
                11,
                SubPatch::Template(vec![Fragment::Lit("a, b)".to_owned())]),
            ),
            (10, 10, SubPatch::Text(", /".to_owned())),
        ];
        assert_eq!(&source[parameters.to_std_range()], "a, b)");
        splice(
            source,
            sub_edits,
            vec![vec![0], vec![1]],
            &[writer, inserter],
        )
        .1
    }

    fn author(lowering: Option<Lowering>, subsumes: &'static [Lowering]) -> Author {
        Author {
            name: "Pass",
            lowering,
            subsumes,
        }
    }

    /// a template that writes a construct itself prints none of the source an edit
    /// inside it was made against, so the edit is refused — whether or not the text the
    /// template writes happens to agree with it
    #[test]
    fn an_insertion_inside_a_construct_a_template_writes_is_refused() {
        let errors = lost_in_a_written_parameter_list(
            author(None, &[]),
            author(Some(Lowering::RepeatedUnderscore), &[]),
        );
        assert_eq!(
            errors,
            [
                "transform conflict: text inserted at byte 10 by `Pass` lands inside a construct `Pass` writes itself, so the lowering would be lost"
            ]
        );
    }

    /// a pass that says it writes a lowering's constructs itself takes that lowering's
    /// edits inside its own
    #[test]
    fn a_lowering_the_template_writes_itself_is_accounted_for() {
        let errors = lost_in_a_written_parameter_list(
            author(None, &[Lowering::RepeatedUnderscore]),
            author(Some(Lowering::RepeatedUnderscore), &[]),
        );
        assert_eq!(errors, Vec::<String>::new());
    }

    /// what a deletion covers is not printed at all, so nothing inside it is lost
    #[test]
    fn a_deletion_takes_the_edits_inside_it() {
        let source = "x: int? = 1\n";
        let sub_edits = vec![
            (1, 7, SubPatch::Text(String::new())),
            (6, 7, SubPatch::Text(" | None".to_owned())),
        ];
        let authors = [author(None, &[]), author(Some(Lowering::OptionalType), &[])];
        let errors = splice(source, sub_edits, vec![vec![0], vec![1]], &authors).1;
        assert_eq!(errors, Vec::<String>::new());
    }

    #[test]
    fn double_coalesce_spliced() {
        let src = "x = None\na = x ?? x ?? \"fallback\"\n";
        let (out, _, _) = run_against_source(
            src,
            repeated_underscore::WrittenNames::new(src),
            &Config::test_default(),
            None,
        );
        assert!(!out.contains("??"), "still has ??: {out}");
    }

    /// a docstring is only a docstring while it is the module's first statement,
    /// so anything generated ahead of one empties the built module's `__doc__`
    #[test]
    fn generated_lines_follow_the_module_docstring() {
        let src = "\"\"\"a module docstring.\"\"\"\n\nlet LIMIT = 10\n";
        let out = crate::transpile(src, &Config::test_default()).unwrap();
        assert!(
            out.starts_with("\"\"\"a module docstring.\"\"\"\n"),
            "got:\n{out}"
        );
        assert!(out.contains("from typing import Final"), "got:\n{out}");
    }

    #[test]
    fn generated_lines_follow_a_future_import_under_the_docstring() {
        let src = "\"\"\"doc.\"\"\"\nfrom __future__ import annotations\n\nlet LIMIT = 10\n";
        let out = crate::transpile(src, &Config::test_default()).unwrap();
        assert!(
            out.starts_with("\"\"\"doc.\"\"\"\nfrom __future__ import annotations\n"),
            "got:\n{out}"
        );
    }

    /// a zero-width insertion exactly at a `Src` span's end must be emitted
    /// when the following fragment is literal text — nothing else can emit it,
    /// so deferring it (the rule for adjacent passthroughs) would lose it
    #[test]
    fn end_boundary_insertion_emitted_before_literal() {
        use ruff_text_size::TextSize;
        let source = "[1]";
        let all = vec![(3usize, 3usize, SubPatch::Text(")".to_owned()))];
        let frags = vec![
            Fragment::Src(TextRange::new(TextSize::from(0u32), TextSize::from(3u32))),
            Fragment::Lit("Y".to_owned()),
        ];
        let mut out = Replacement::default();
        materialize_fragments(&mut out, &frags, source, &all, &[0], 0, &mut [false]);
        assert_eq!(out.text(), "[1])Y");
    }

    /// between two adjacent `Src` spans the shared boundary insertion is
    /// emitted only by the second (at its start), never twice
    #[test]
    fn shared_boundary_insertion_emitted_once() {
        use ruff_text_size::TextSize;
        let source = "[1]W";
        let all = vec![(3usize, 3usize, SubPatch::Text(")".to_owned()))];
        let frags = vec![
            Fragment::Src(TextRange::new(TextSize::from(0u32), TextSize::from(3u32))),
            Fragment::Src(TextRange::new(TextSize::from(3u32), TextSize::from(4u32))),
        ];
        let mut out = Replacement::default();
        materialize_fragments(&mut out, &frags, source, &all, &[0], 0, &mut [false]);
        assert_eq!(out.text(), "[1])W");
    }
}

/// a pass that declares it writes another lowering itself
/// ([`TypeAwarePass::subsumes`]) is trusted to: the edits that lowering makes inside
/// one of the pass's own are left out of the output without an error. a declaration
/// the pass does not live up to drops the lowering in silence, so each one is shown
/// here written, by a construct of the declaring pass with the lowered construct
/// inside it, unless [`SHOWN_ELSEWHERE`](subsumed_lowerings::SHOWN_ELSEWHERE) says where
/// it is shown. a declaration with neither fails
/// `every_declared_lowering_is_shown_written`
#[cfg(test)]
mod subsumed_lowerings {
    use ruff_python_ast::PythonVersion;

    use super::{DECLARED_SUBSUMPTIONS, Lowering};
    use crate::{Config, transpile};

    /// the declared pairs shown written somewhere other than a case here, by the declaring
    /// pass's name: the test of the pass's own that shows it, or why it holds
    const SHOWN_ELSEWHERE: &[(&str, Lowering, &str)] = &[
        (
            "GenericPolyfillPass",
            Lowering::MatchType,
            "polyfilled_match_type_lowers_to_object",
        ),
        (
            "GenericPolyfillPass",
            Lowering::SymbolicTypeOp,
            "arithmetic_type_alias_is_folded_too",
        ),
        (
            "GenericPolyfillPass",
            Lowering::Modifiers,
            "private_match_type_takes_the_underscore_name",
        ),
        (
            "GenericPolyfillPass",
            Lowering::VisibilityRename,
            "private_match_type_takes_the_underscore_name",
        ),
        (
            "ModifiersPass",
            Lowering::StatementExpression,
            "a_declaration_takes_a_suite_bearing_value",
        ),
        (
            "ExtensionBlockPass",
            Lowering::Modifiers,
            "static_and_class_members_bind_the_class_object",
        ),
        (
            "ExtensionBlockPass",
            Lowering::ContextParams,
            "a_context_parameter_of_an_extension_member_is_filled",
        ),
        (
            "ExtensionBlockPass",
            Lowering::GenericPolyfill,
            "conditional_extension_keeps_bounds_in_marker",
        ),
        (
            "ExtensionBlockPass",
            Lowering::RepeatedUnderscore,
            "a_repeated_underscore_in_a_member_is_numbered",
        ),
        (
            "DecoratorKeyword",
            Lowering::LocalOnce,
            "a_parameter_modifier_leaves_the_overloads_as_they_were",
        ),
        (
            "DecoratorKeyword",
            Lowering::ContextParams,
            "a_parameter_modifier_leaves_the_overloads_as_they_were",
        ),
        (
            "KwSubscriptPass",
            Lowering::OptionalType,
            "getitem_kw_value_optional_lowers",
        ),
        (
            "MatchTypePass",
            Lowering::Unpack,
            "polyfilled_match_type_lowers_to_object",
        ),
        (
            "VisibilityRenamePass",
            Lowering::Modifiers,
            "private_type_alias_polyfilled",
        ),
        ("PropertiesPass", Lowering::Modifiers, "stored_var_property"),
        (
            "SymbolicTypeOp",
            Lowering::LiteralType,
            "plain_int_addition",
        ),
        (
            "SymbolicTypeOp",
            Lowering::DynamicKeyword,
            "dynamic_operand_folds_to_any",
        ),
        ("SymbolicTypeOp", Lowering::Typeof, "typeof_operand"),
        (
            "SymbolicTypeOp",
            Lowering::Callable,
            "by construction: a fold writes the type ty computed and none of its operands",
        ),
        (
            "SymbolicTypeOp",
            Lowering::OptionalType,
            "by construction: a fold writes the type ty computed and none of its operands",
        ),
        (
            "SymbolicTypeOp",
            Lowering::JustFloat,
            "by construction: a fold writes the type ty computed and none of its operands",
        ),
        (
            "SymbolicTypeOp",
            Lowering::FloatConst,
            "by construction: a fold writes the type ty computed and none of its operands",
        ),
        (
            "ParametricIsPass",
            Lowering::LiteralType,
            "protocol_int_literal_argument",
        ),
        // the runtime-union lowering runs only below 3.10, where the cases above never go
        (
            "CallableSyntaxPass",
            Lowering::RuntimeUnion,
            "a_cast_target_and_a_typevar_bound_are_spelled_out_below_310",
        ),
    ];

    /// `source`, which puts a construct of the pass named `pass` around one of each of
    /// `lowerings`, transpiles to python holding each of `written` and none of `unwritten`
    struct Case {
        pass: &'static str,
        lowerings: Vec<Lowering>,
        source: String,
        written: Vec<String>,
        unwritten: Vec<String>,
    }

    fn case(
        pass: &'static str,
        lowerings: &[Lowering],
        source: &str,
        written: &[&str],
        unwritten: &[&str],
    ) -> Case {
        Case {
            pass,
            lowerings: lowerings.to_vec(),
            source: source.to_owned(),
            written: written.iter().map(|text| (*text).to_owned()).collect(),
            unwritten: unwritten.iter().map(|text| (*text).to_owned()).collect(),
        }
    }

    /// a type expression of each of `lowerings`, as `wrap` puts it inside a construct of
    /// `pass`, is printed lowered where `show` says the construct puts it
    fn leaves(
        pass: &'static str,
        lowerings: &[Lowering],
        wrap: impl Fn(&str) -> String,
        show: impl Fn(&str) -> String,
    ) -> Vec<Case> {
        let leaves = [
            (Lowering::LiteralType, "1", "Literal[1]", None),
            (Lowering::JustFloat, "float", "JustFloat", None),
            (Lowering::DynamicKeyword, "dynamic", "Any", Some("dynamic")),
            (Lowering::FloatConst, "1.5", "float", Some("1.5")),
            (Lowering::OptionalType, "int?", "int | None", Some("int?")),
            (
                Lowering::Callable,
                "(int) -> str",
                "Callable[[int], str]",
                Some("(int) ->"),
            ),
            (Lowering::Typeof, "typeof y", "TypeOf[y]", Some("typeof")),
        ];
        leaves
            .into_iter()
            .filter(|(lowering, ..)| lowerings.contains(lowering))
            .map(|(lowering, leaf, lowered, unwritten)| Case {
                pass,
                lowerings: vec![lowering],
                source: format!("y = 1\n{}\n", wrap(leaf)),
                written: vec![show(lowered)],
                unwritten: unwritten.map(str::to_owned).into_iter().collect(),
            })
            .collect()
    }

    fn check(cases: &[Case], version: PythonVersion) {
        let config = Config {
            min_version: version,
            ..Config::test_default()
        };
        let mut failures = Vec::new();
        for case in cases {
            let what = format!("`{}` around {:?}", case.pass, case.lowerings);
            let output = match transpile(&case.source, &config) {
                Ok(output) => output,
                Err(error) => {
                    failures.push(format!("{what}: refused: {error}"));
                    continue;
                }
            };
            for text in &case.written {
                if !output.contains(text.as_str()) {
                    failures.push(format!("{what}: `{text}` is missing from\n{output}"));
                }
            }
            for text in &case.unwritten {
                if output.contains(text.as_str()) {
                    failures.push(format!("{what}: `{text}` is left in\n{output}"));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    }

    fn type_expression_cases() -> Vec<Case> {
        let mut cases = Vec::new();
        cases.extend(leaves(
            "TupleLiteralTypePass",
            super::super::callable::TYPE_EXPRESSION,
            |leaf| format!("def f(x: ({leaf}, str)) -> None: ..."),
            |lowered| format!("tuple[{lowered}, str]"),
        ));
        cases.extend(leaves(
            "CallableSyntaxPass",
            super::super::callable::TYPE_EXPRESSION,
            |leaf| format!("def f(x: ({leaf}) -> str) -> None: ..."),
            |lowered| format!("Callable[[{lowered}], str]"),
        ));
        let fields = [
            Lowering::Callable,
            Lowering::OptionalType,
            Lowering::LiteralType,
            Lowering::JustFloat,
            Lowering::DynamicKeyword,
            Lowering::FloatConst,
            Lowering::Typeof,
        ];
        cases.extend(leaves(
            "AnonNamedTuplePass",
            &fields,
            |leaf| format!("def f(x: (a: {leaf}, b: str)) -> None: ..."),
            |lowered| format!("a: {lowered}"),
        ));
        cases.extend(leaves(
            "ProtocolTypePass",
            &fields,
            |leaf| format!("def f(x: protocol(a: {leaf})) -> None: ..."),
            |lowered| format!("a: \"{lowered}\""),
        ));
        cases.extend(leaves(
            "TypedDictLiteralPass",
            &fields,
            |leaf| format!("def f(x: {{\"a\": {leaf}}}) -> None: ..."),
            |lowered| format!("a: \"{lowered}\""),
        ));
        cases.push(case(
            "CallableSyntaxPass",
            &[Lowering::ProtocolType],
            "def f(x: (protocol(a: int)) -> str) -> None: ...\n",
            &["Callable[[_Protocol_"],
            &["protocol("],
        ));
        cases
    }

    fn whole_construct_cases() -> Vec<Case> {
        vec![
            case(
                "GenericPolyfillPass",
                &[Lowering::TupleLiteralType],
                "type Pair = (int, str)\n",
                &["TypeAliasType(\"Pair\", tuple[int, str])"],
                &["(int, str)"],
            ),
            case(
                "GenericPolyfillPass",
                &[Lowering::AnonNamedTuple],
                "type Point = (x: int, y: int)\n",
                &["TypeAliasType(\"Point\", _AnonNamedTuple_"],
                &["(x: int"],
            ),
            case(
                "GenericPolyfillPass",
                &[Lowering::VarianceStrip],
                "class Box[out T]:\n    def get(self) -> T: ...\n",
                &["covariant=True"],
                &["out T"],
            ),
            case(
                "ExtensionBlockPass",
                &[
                    Lowering::LiteralType,
                    Lowering::JustFloat,
                    Lowering::DynamicKeyword,
                    Lowering::FloatConst,
                    Lowering::OptionalType,
                    Lowering::Callable,
                ],
                "extension int:\n    def scaled(self, by: 1, f: float, d: dynamic, c: 1.5, o: int?, k: (int) -> str) -> float:\n        return 1.0\n",
                &["(self, by, f, d, c, o, k)"],
                &["dynamic", "1.5", "int?", "(int) ->"],
            ),
            case(
                "PropertiesPass",
                &[Lowering::InferredAnnotation],
                "class A:\n    var v = 0\n        get() = field\n        set(value):\n            field = value\n",
                &["self.__v: int = 0"],
                &["var v"],
            ),
            case(
                "TypeIs",
                &[Lowering::ParametricIs],
                "def f(x: object) -> x is list[int]:\n    return isinstance(x, list)\n",
                &["TypeIs[list[int]]"],
                &["is list"],
            ),
            case(
                "StatementExpressionPass",
                &[Lowering::NoneCoalesce],
                "def f(xs: list[int?]) -> None:\n    for x in xs:\n        y = x ?? continue\n        print(y)\n",
                &["continue"],
                &["??"],
            ),
            case(
                "NoneCoalescePass",
                &[Lowering::NoneChain],
                "class A:\n    b: int? = None\n\ndef f(a: A?) -> int:\n    return a?.b ?? 1\n",
                &[],
                &["?.", "??"],
            ),
        ]
    }

    #[test]
    fn a_type_expression_a_pass_moves_is_written_lowered() {
        check(&type_expression_cases(), PythonVersion::PY310);
    }

    #[test]
    fn a_construct_a_pass_writes_whole_is_written_lowered() {
        check(&whole_construct_cases(), PythonVersion::PY310);
    }

    /// every lowering a pass declares it writes has a case above or an entry in
    /// [`SHOWN_ELSEWHERE`], so a declaration added later fails here until something shows
    /// it written — and an entry whose declaration is gone fails too
    #[test]
    fn every_declared_lowering_is_shown_written() {
        DECLARED_SUBSUMPTIONS.with(|declared| declared.borrow_mut().clear());
        transpile("x = 1\n", &Config::test_default()).expect("transpile failed");
        let declared = DECLARED_SUBSUMPTIONS.with(std::cell::RefCell::take);
        let cases: Vec<Case> = type_expression_cases()
            .into_iter()
            .chain(whole_construct_cases())
            .collect();

        let mut failures = Vec::new();
        for &(pass, lowerings) in &declared {
            for lowering in lowerings {
                let in_a_case = cases
                    .iter()
                    .any(|case| case.pass == pass && case.lowerings.contains(lowering));
                let elsewhere = SHOWN_ELSEWHERE
                    .iter()
                    .any(|(shown, shown_lowering, _)| *shown == pass && shown_lowering == lowering);
                if !in_a_case && !elsewhere {
                    failures.push(format!(
                        "`{pass}` declares it writes {lowering:?}, and nothing shows it"
                    ));
                }
            }
        }
        for (pass, lowering, _) in SHOWN_ELSEWHERE {
            if !declared
                .iter()
                .any(|(name, lowerings)| name == pass && lowerings.contains(lowering))
            {
                failures.push(format!(
                    "`{pass}` no longer declares {lowering:?}, which `SHOWN_ELSEWHERE` lists"
                ));
            }
        }
        for case in &cases {
            if !declared.iter().any(|(name, lowerings)| {
                *name == case.pass
                    && case
                        .lowerings
                        .iter()
                        .all(|lowering| lowerings.contains(lowering))
            }) {
                failures.push(format!(
                    "a case shows `{}` writing {:?}, which it does not declare",
                    case.pass, case.lowerings
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
